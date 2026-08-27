/// Coarse language classification. The pipeline only needs to know whether
/// S1-mini is operating in-domain (English) or out of it — see spec 7.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    English,
    Other,
}
