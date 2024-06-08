mod language_servers;

use language_servers::hls::Hls;
use language_servers::static_ls::StaticLs;
use zed::lsp::Symbol;
use zed::CodeLabel;
use zed_extension_api::{self as zed, Result};

struct HaskellExtension {
    static_ls: Option<StaticLs>,
    hls: Option<Hls>,
}

impl zed::Extension for HaskellExtension {
    fn new() -> Self {
        Self {
            static_ls: None,
            hls: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        match language_server_id.as_ref() {
            Hls::LANGUAGE_SERVER_ID => {
                let _hls = self.hls.get_or_insert_with(|| Hls::new());
                let env = worktree.shell_env();
                let path = worktree
                    .which("haskell-language-server-wrapper")
                    .ok_or_else(|| "hls must be installed via ghcup".to_string())?;

                Ok(zed::Command {
                    command: path,
                    args: vec!["lsp".to_string()],
                    env,
                })
            }
            StaticLs::LANGUAGE_SERVER_ID => {
                let _static_ls = self.static_ls.get_or_insert_with(|| StaticLs::new());
                let env = worktree.shell_env();
                let path = worktree
                    .which("static-ls")
                    .ok_or_else(|| "static-ls could not be found")?;
                Ok(zed::Command {
                    command: path,
                    args: vec![],
                    env,
                })
            }
            language_server_id => Err(format!("unknown language server: {language_server_id}")),
        }
    }

    fn label_for_symbol(
        &self,
        language_server_id: &zed::LanguageServerId,
        symbol: Symbol,
    ) -> Option<CodeLabel> {
        match language_server_id.as_ref() {
            Hls::LANGUAGE_SERVER_ID => self.hls.as_ref()?.label_for_symbol(symbol),
            StaticLs::LANGUAGE_SERVER_ID => self.static_ls.as_ref()?.label_for_symbol(symbol),
            _ => None,
        }
    }
}

zed::register_extension!(HaskellExtension);
