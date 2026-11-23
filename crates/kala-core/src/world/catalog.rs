//! The catalog: what a world's names and IDs mean.

use super::*;

/// A world's schema and ID tables: node types, attributes (with their
/// history per canon), settings, entity handles and interned edges. Rebuilt
/// from the log's `DEFINE` and schema entries on open; it holds no facts
/// (those are in [`Indexes`]).
#[derive(Debug)]
pub(super) struct Catalog {
    /// Indexed by `NodeTypeId.0`.
    pub(super) node_types: Vec<NodeType>,
    pub(super) node_type_by_name: HashMap<String, NodeTypeId>,
    pub(super) entity_by_handle: HashMap<String, EntityId>,
    /// The reverse of `entity_by_handle`, for [`World::handle_of`].
    pub(super) handle_by_entity: HashMap<EntityId, String>,
    /// Indexed by `AttrId.0`.
    pub(super) attributes: Vec<AttrDef>,
    pub(super) attribute_history: Vec<Vec<(TxId, AttrDef)>>,
    pub(super) node_origins: Vec<Vec<(TxId, bool)>>,
    pub(super) settings: Vec<(TxId, String, String)>,
    pub(super) attr_by_name: HashMap<String, AttrId>,
    /// Inverse name -> the directed attribute it reverses. Not attribute
    /// names: every API except [`View::neighbors`] treats them as unknown
    /// attributes.
    pub(super) inverse_by_name: HashMap<String, AttrId>,
    /// Interned edges: entry `i` is the triple of the edge with seq `i + 1`
    /// (edge sequences are assigned without gaps).
    pub(super) edges: Vec<Triple>,
    /// The transaction that defined each edge, parallel to `edges` (so
    /// ascending): what [`View::edge`] checks against its canon.
    pub(super) edges_defined_in: Vec<TxId>,
    pub(super) edge_by_triple: HashMap<Triple, EdgeId>,
}
