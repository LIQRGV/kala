//! Language-neutral catalogue and adapter-supplied row requests.
/// A server's database listing selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DatabaseSelection {
    All,
    Named(String),
    Default,
    Home,
}
/// Metadata that only the serving adapter can answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerRequest {
    Components,
    Databases(DatabaseSelection),
}
/// A read-only row source. Schema sources read the pinned canon and tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowSource {
    Labels,
    RelationshipTypes,
    PropertyKeys,
    Server(ServerRequest),
}
impl RowSource {
    /// Stable output column names, in protocol order.
    pub fn columns(&self) -> &'static [&'static str] {
        match self {
            Self::Labels => &["label"],
            Self::RelationshipTypes => &["relationshipType"],
            Self::PropertyKeys => &["propertyKey"],
            Self::Server(ServerRequest::Components) => &["name", "versions", "edition"],
            Self::Server(ServerRequest::Databases(_)) => &[
                "name",
                "type",
                "aliases",
                "access",
                "address",
                "role",
                "writer",
                "requestedStatus",
                "currentStatus",
                "statusMessage",
                "default",
                "home",
                "constituents",
            ],
        }
    }
    /// Stable explain spelling, independent of a language's syntax.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Labels => "labels",
            Self::RelationshipTypes => "relationship-types",
            Self::PropertyKeys => "property-keys",
            Self::Server(ServerRequest::Components) => "server-components",
            Self::Server(ServerRequest::Databases(_)) => "server-databases",
        }
    }
}
/// Reserved adapter input, impossible to reference as an ordinary parameter.
/// It contains a list of rows (each a list); adapters supply it after preparation.
pub const SERVER_ROWS_PARAMETER: &str = "\0server-rows";
