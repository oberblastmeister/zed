use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use editor::{
    display_map::{
        BlockContext, BlockDisposition, BlockId, BlockProperties, BlockStyle, RenderBlock,
    },
    Anchor, AnchorRangeExt, Editor,
};
use futures::{
    channel::mpsc::{self, UnboundedSender},
    Future, SinkExt as _, StreamExt as _,
};
use gpui::{actions, AppContext, Context, EntityId, Global, Model, ModelContext, Task, WeakView};
use gpui::{Entity, View};
use kernelspecs::{get_runtimes, RunningKernel, Runtime};
use language::Point;
use outputs::{ExecutionStatus, ExecutionView, LineHeight as _};
use project::Fs;
use settings::Settings as _;
use std::ops::Range;
use std::sync::Arc;
use theme::{ActiveTheme, ThemeSettings};
use tokio_kernel::{ExecutionId, Request, Update};
use ui::prelude::*;
use workspace::Workspace;

mod kernelspecs;
mod outputs;
mod stdio;
mod tokio_kernel;

actions!(runtimes, [Run]);

#[derive(Clone)]
pub struct RuntimeGlobal(Model<RuntimeManager>);

impl Global for RuntimeGlobal {}

pub fn init(fs: Arc<dyn Fs>, cx: &mut AppContext) {
    let runtime_manager = cx.new_model(|cx| RuntimeManager::new(fs.clone(), cx));
    RuntimeManager::set_global(runtime_manager.clone(), cx);

    cx.spawn(|mut cx| async move {
        let fs = fs.clone();

        let runtimes = get_runtimes(fs).await?;

        runtime_manager.update(&mut cx, |this, _cx| {
            this.runtimes = runtimes;
        })?;

        anyhow::Ok(())
    })
    .detach_and_log_err(cx);

    cx.observe_new_views(
        |workspace: &mut Workspace, _: &mut ViewContext<Workspace>| {
            // Note: this will have to both start a kernel if not already running, and run code selections
            workspace.register_action(run);
        },
    )
    .detach();
}

// Per workspace
pub struct RuntimeManager {
    fs: Arc<dyn Fs>,
    // Things we can connect to
    runtimes: Vec<Runtime>, // specification for what's available to run (kernelspec)

    // Editor -> Running Kernel
    // Connections
    instances: HashMap<EntityId, RunningKernel>, // actually running kernels
    editors: HashMap<WeakView<Editor>, EditorRuntimeState>,
}

// We will store the blocks

// Store all the blocks we're working with so that we can
// * Remove them when

#[derive(Debug, Clone)]
struct EditorRuntimeState {
    // Could keep this as a sorted list of blocks so that we can eliminate
    // blocks that overlap with each other
    blocks: Vec<EditorRuntimeBlock>,
    // Store a subscription to the editor so we can drop them when the editor is dropped
    // subscription: gpui::Subscription,
}

#[derive(Debug, Clone)]
struct EditorRuntimeBlock {
    code_range: Range<Anchor>,
    block_id: BlockId,
    _execution_view: View<ExecutionView>,
}

impl RuntimeManager {
    pub fn new(fs: Arc<dyn Fs>, _cx: &mut AppContext) -> Self {
        Self {
            fs,
            runtimes: Default::default(),
            instances: Default::default(),
            editors: Default::default(),
        }
    }

    fn acquire_shell_request_tx(
        &mut self,
        entity_id: EntityId,
        language_name: Arc<str>,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<UnboundedSender<Request>>> {
        let running_kernel = self.instances.get(&entity_id);
        if let Some(running_kernel) = running_kernel {
            return Task::ready(anyhow::Ok(running_kernel.shell_request_tx.clone()));
        }
        // TODO: Track that a kernel is (possibly) starting up so we don't relaunch without tearing down the old one

        // Get first runtime that matches the language name (for now)
        let runtime = self
            .runtimes
            .iter()
            .find(|runtime| runtime.spec.language == language_name.to_string());

        let runtime = match runtime {
            Some(runtime) => runtime,
            None => {
                return Task::ready(Err(anyhow::anyhow!(
                    "No runtime found for language {}",
                    language_name
                )));
            }
        };

        let runtime = runtime.clone();

        let fs = self.fs.clone();

        cx.spawn(|this, mut cx| async move {
            let running_kernel = RunningKernel::new(runtime, &entity_id, fs.clone()).await?;

            let mut shell_request_tx = running_kernel.shell_request_tx.clone();
            let (tx, mut rx) = mpsc::unbounded();
            shell_request_tx
                .send(Request {
                    execution_id: ExecutionId::new(),
                    request: runtimelib::JupyterMessageContent::KernelInfoRequest(
                        runtimelib::KernelInfoRequest {},
                    ),
                    iopub_sender: tx,
                })
                .await?;

            // Wait for a kernel info reply on launch
            let timeout = smol::Timer::after(std::time::Duration::from_secs(1));
            futures::future::select(rx.next(), timeout).await;

            let shell_request_tx = running_kernel.shell_request_tx.clone();
            this.update(&mut cx, |this, _cx| {
                this.instances.insert(entity_id, running_kernel);
                anyhow::Ok(())
            })??;

            anyhow::Ok(shell_request_tx)
        })
    }

    fn execute_code(
        &mut self,
        entity_id: EntityId,
        language_name: Arc<str>,
        execution_id: ExecutionId,
        code: String,
        cx: &mut ModelContext<Self>,
    ) -> impl Future<Output = Result<mpsc::UnboundedReceiver<Update>>> {
        let (tx, rx) = mpsc::unbounded();

        let shell_request_tx = self.acquire_shell_request_tx(entity_id, language_name, cx);

        async move {
            let shell_request_tx = shell_request_tx.await?;

            shell_request_tx
                .unbounded_send(Request {
                    execution_id,
                    request: runtimelib::JupyterMessageContent::ExecuteRequest(
                        runtimelib::ExecuteRequest {
                            code,
                            allow_stdin: false,
                            silent: false,
                            store_history: true,
                            user_expressions: None,
                            stop_on_error: true,
                            // TODO(runtimelib): set up Default::default() for the rest of the fields
                            // ..Default::default()
                        },
                    ),
                    iopub_sender: tx,
                })
                .context("Failed to send execution request")?;

            Ok(rx)
        }
    }

    pub fn global(cx: &AppContext) -> Option<Model<Self>> {
        cx.try_global::<RuntimeGlobal>()
            .map(|model| model.0.clone())
    }

    pub fn set_global(runtime: Model<Self>, cx: &mut AppContext) {
        cx.set_global(RuntimeGlobal(runtime));
    }
}

pub fn get_active_editor(
    workspace: &mut Workspace,
    cx: &mut ViewContext<Workspace>,
) -> Option<View<Editor>> {
    workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
}

// Gets the active selection in the editor or the current line
pub fn selection(editor: View<Editor>, cx: &mut ViewContext<Workspace>) -> Range<Anchor> {
    let editor = editor.read(cx);
    let selection = editor.selections.newest::<usize>(cx);
    let buffer = editor.buffer().read(cx).snapshot(cx);

    let range = if selection.is_empty() {
        let cursor = selection.head();

        let line_start = buffer.offset_to_point(cursor).row;
        let mut start_offset = buffer.point_to_offset(Point::new(line_start, 0));

        // Iterate backwards to find the start of the line
        while start_offset > 0 {
            let ch = buffer.chars_at(start_offset - 1).next().unwrap_or('\0');
            if ch == '\n' {
                break;
            }
            start_offset -= 1;
        }

        let mut end_offset = cursor;

        // Iterate forwards to find the end of the line
        while end_offset < buffer.len() {
            let ch = buffer.chars_at(end_offset).next().unwrap_or('\0');
            if ch == '\n' {
                break;
            }
            end_offset += 1;
        }

        // Create a range from the start to the end of the line
        start_offset..end_offset
    } else {
        selection.range()
    };

    let anchor_range = buffer.anchor_before(range.start)..buffer.anchor_after(range.end);
    anchor_range
}

pub fn run(workspace: &mut Workspace, _: &Run, cx: &mut ViewContext<Workspace>) {
    let (editor, runtime_manager) = if let (Some(editor), Some(runtime_manager)) =
        (get_active_editor(workspace, cx), RuntimeManager::global(cx))
    {
        (editor, runtime_manager)
    } else {
        log::warn!("No active editor or runtime manager found");
        return;
    };

    let anchor_range = selection(editor.clone(), cx);

    let buffer = editor.read(cx).buffer().read(cx).snapshot(cx);

    let selected_text = buffer
        .text_for_range(anchor_range.clone())
        .collect::<String>();

    let start_language = buffer.language_at(anchor_range.start);
    let end_language = buffer.language_at(anchor_range.end);

    let language_name = if start_language == end_language {
        start_language
            .map(|language| language.code_fence_block_name())
            .filter(|lang| **lang != *"markdown")
    } else {
        // If the selection spans multiple languages, don't run it
        return;
    };

    let language_name = if let Some(language_name) = language_name {
        language_name
    } else {
        return;
    };

    let entity_id = editor.entity_id();
    let execution_id = ExecutionId::new();

    // Since we don't know the height, in editor terms, we have to calculate it over time
    // and just create a new block, replacing the old. It would be better if we could
    // just rely on the view updating and for the height to be calculated automatically.
    //
    // We will just handle text for the moment to keep this accurate.
    // Plots and other images will have to wait.
    let execution_view = cx.new_view(|cx| ExecutionView::new(execution_id.clone(), cx));

    // If any block overlaps with the new block, remove it
    // When inserting a new block, put it in order so that search is efficient
    let blocks_to_remove = runtime_manager.update(cx, |runtime_manager, _cx| {
        // Get the current `EditorRuntimeState` for this runtime_manager, inserting it if it doesn't exist
        let editor_runtime_state = runtime_manager
            .editors
            .entry(editor.downgrade())
            .or_insert_with(|| EditorRuntimeState { blocks: Vec::new() });

        let mut blocks_to_remove: HashSet<BlockId> = HashSet::default();
        for (_i, block) in editor_runtime_state.blocks.iter().enumerate() {
            let other_range: Range<Anchor> = block.code_range.clone();

            if anchor_range.overlaps(&other_range, &buffer) {
                blocks_to_remove.insert(block.block_id);
            }
        }

        blocks_to_remove
    });

    let blocks_to_remove = blocks_to_remove.clone();

    let mut block_id = editor.update(cx, |editor, cx| {
        println!("Removing blocks {blocks_to_remove:?}");
        editor.remove_blocks(blocks_to_remove, None, cx);
        let block = BlockProperties {
            position: anchor_range.end,
            height: execution_view.num_lines(cx).saturating_add(1),
            style: BlockStyle::Sticky,
            render: create_output_area_render(execution_view.clone()),
            disposition: BlockDisposition::Below,
        };

        editor.insert_blocks([block], None, cx)[0]
    });

    println!("Created block {block_id:?}");

    let (receiver, editor_runtime_block) = runtime_manager.update(cx, |runtime_manager, cx| {
        let editor_runtime_block = EditorRuntimeBlock {
            code_range: anchor_range.clone(),
            block_id,
            _execution_view: execution_view.clone(),
        };

        let editor_runtime_state = runtime_manager
            .editors
            .entry(editor.downgrade())
            .or_insert_with(|| EditorRuntimeState { blocks: Vec::new() });

        editor_runtime_state
            .blocks
            .push(editor_runtime_block.clone());

        // Run the code!
        (
            runtime_manager.execute_code(
                entity_id,
                language_name,
                execution_id.clone(),
                selected_text.clone(),
                cx,
            ),
            editor_runtime_block,
        )
    });

    cx.spawn(|_this, mut cx| async move {
        execution_view.update(&mut cx, |execution_view, cx| {
            execution_view.set_status(ExecutionStatus::ConnectingToKernel, cx);
        })?;
        let mut receiver = receiver.await?;

        let execution_view = execution_view.clone();
        while let Some(update) = receiver.next().await {
            {}

            execution_view.update(&mut cx, |execution_view, cx| {
                execution_view.push_message(&update.content, cx)
            })?;

            let block_id = editor.update(&mut cx, |editor, cx| {
                let mut blocks_to_remove = HashSet::default();
                blocks_to_remove.insert(block_id);

                editor.remove_blocks(blocks_to_remove, None, cx);

                let block = BlockProperties {
                    position: anchor_range.end,
                    height: execution_view.num_lines(cx).saturating_add(1),
                    style: BlockStyle::Sticky,
                    render: create_output_area_render(execution_view.clone()),
                    disposition: BlockDisposition::Below,
                };

                block_id = editor.insert_blocks([block], None, cx)[0];

                block_id
            })?;

            // runtime_manager.update(&mut cx, |runtime_manager, cx| {
            //     editor_runtime_block.block_id = block_id;
            //     // runtime_manager.update_block(editor_runtime_block, update, cx);
            // });
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn create_output_area_render(execution_view: View<ExecutionView>) -> RenderBlock {
    let render = move |cx: &mut BlockContext| {
        let execution_view = execution_view.clone();
        let text_font = ThemeSettings::get_global(cx).buffer_font.family.clone();
        // Note: we'll want to use `cx.anchor_x` when someone runs something with no output -- just show a checkmark and not make the full block below the line

        let gutter_width = cx.gutter_dimensions.width;

        h_flex()
            .w_full()
            .bg(cx.theme().colors().background)
            .border_y_1()
            .border_color(cx.theme().colors().border)
            .pl(gutter_width)
            .child(
                div()
                    .font_family(text_font)
                    // .ml(gutter_width)
                    .mx_1()
                    .my_2()
                    .h_full()
                    .w_full()
                    .mr(gutter_width)
                    .child(execution_view),
            )
            .into_any_element()
    };

    Box::new(render)
}
