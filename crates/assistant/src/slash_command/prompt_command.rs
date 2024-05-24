use super::{SlashCommand, SlashCommandOutput};
use crate::prompts::prompt_library::PromptLibrary;
use anyhow::{anyhow, Context, Result};
use fuzzy::StringMatchCandidate;
use gpui::{prelude::*, AppContext, Task};
use std::sync::{atomic::AtomicBool, Arc};
use ui::{h_flex, Icon, IconName};

pub(crate) struct PromptSlashCommand {
    library: Arc<PromptLibrary>,
}

impl PromptSlashCommand {
    pub fn new(library: Arc<PromptLibrary>) -> Self {
        Self { library }
    }
}

impl SlashCommand for PromptSlashCommand {
    fn name(&self) -> String {
        "prompt".into()
    }

    fn description(&self) -> String {
        "insert a prompt from the library".into()
    }

    fn requires_argument(&self) -> bool {
        true
    }

    fn complete_argument(
        &self,
        query: String,
        cancellation_flag: Arc<AtomicBool>,
        cx: &mut AppContext,
    ) -> Task<Result<Vec<String>>> {
        let library = self.library.clone();
        let executor = cx.background_executor().clone();
        cx.background_executor().spawn(async move {
            let candidates = library
                .prompts()
                .into_iter()
                .enumerate()
                .filter_map(|(ix, prompt)| {
                    prompt
                        .1
                        .title()
                        .map(|title| StringMatchCandidate::new(ix, title.into()))
                })
                .collect::<Vec<_>>();
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                false,
                100,
                &cancellation_flag,
                executor,
            )
            .await;
            Ok(matches
                .into_iter()
                .map(|mat| candidates[mat.candidate_id].string.clone())
                .collect())
        })
    }

    fn run(&self, title: Option<&str>, cx: &mut AppContext) -> Task<Result<SlashCommandOutput>> {
        let Some(title) = title else {
            return Task::ready(Err(anyhow!("missing prompt name")));
        };

        let library = self.library.clone();
        let title = title.to_string();
        let prompt = cx.background_executor().spawn({
            let title = title.clone();
            async move {
                let prompt = library
                    .prompts()
                    .into_iter()
                    .filter_map(|prompt| prompt.1.title().map(|title| (title, prompt)))
                    .find(|(t, _)| t == &title)
                    .with_context(|| format!("no prompt found with title {:?}", title))?
                    .1;
                anyhow::Ok(prompt.1.content().to_owned())
            }
        });
        cx.foreground_executor().spawn(async move {
            let prompt = prompt.await?;
            Ok(SlashCommandOutput {
                text: prompt,
                render_placeholder: Arc::new(move |id, unfold, _cx| {
                    h_flex()
                        .child(Icon::new(IconName::Library))
                        .child(title.clone())
                        .into_any()
                }),
            })
        })
    }
}
