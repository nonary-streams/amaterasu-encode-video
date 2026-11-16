//! Test-only handle to the ordinary managed Video module.
use crate::Video;
use amaterasu_module_sdk::{ManagedModule, ModuleResult, RuntimeClient};

pub struct VideoModule {
    module: Video,
}
impl VideoModule {
    pub fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        Ok(Self {
            module: Video::start(runtime)?,
        })
    }
    pub fn tick(&mut self) -> ModuleResult<()> {
        self.module.tick()
    }
}
