#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PolicyTraceContext {
    pub(crate) retry_count: u32,
    pub(crate) redirect_count: u32,
}
