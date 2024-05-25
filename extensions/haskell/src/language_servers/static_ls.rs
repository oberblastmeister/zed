use zed_extension_api::{lsp::Symbol, CodeLabel};

pub struct StaticLs {}

impl StaticLs {
    pub const LANGUAGE_SERVER_ID: &'static str = "static-ls";
    
    pub fn new() -> Self {
        Self {}
    }
    
    pub fn label_for_symbol(&self, _symbol: Symbol) -> Option<CodeLabel> {
        None
    }
}