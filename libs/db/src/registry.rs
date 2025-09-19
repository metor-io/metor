use metor_proto::{
    table::{Entry, VTable},
    types::PacketId,
};
use std::collections::HashMap;

#[derive(Default)]
pub struct VTableRegistry {
    pub map: HashMap<PacketId, VTable>,
}

impl metor_proto::registry::VTableRegistry for VTableRegistry {
    type EntryBuf = Vec<Entry>;

    type DataBuf = Vec<u8>;

    fn get(
        &self,
        id: &PacketId,
    ) -> Option<&metor_proto::table::VTable<Self::EntryBuf, Self::DataBuf>> {
        self.map.get(id)
    }
}
