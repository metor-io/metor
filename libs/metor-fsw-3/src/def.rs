//! The instance config a type computes its [`SystemDef`](crate::SystemDef) from.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::coordinator::OutputConfig;
use crate::system::PortDef;

/// What a type sees of one instance's config when it computes its def.
pub struct DefCx<'a> {
    /// One entry per config edge: the input port it names and the def of the
    /// output feeding it.
    pub inputs: &'a [(&'a str, &'a PortDef)],
    /// Each config output port and the record it names.
    pub outputs: &'a [OutputConfig],
    /// Every record the table knows.
    pub records: &'a Records,
}

static NO_RECORDS: LazyLock<Records> = LazyLock::new(Records::default);

impl DefCx<'static> {
    /// The context with nothing in it: the def the descriptor carries.
    pub fn empty() -> Self {
        Self {
            inputs: &[],
            outputs: &[],
            records: &NO_RECORDS,
        }
    }
}

/// A `DefError` is why a type refused its instance config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum DefError {
    #[error("input `{port}` takes one producer")]
    FanIn { port: String },

    #[error("output `{port}` names record `{record}`, which no registered port declares")]
    UnknownRecord { port: String, record: String },

    #[error("record `{record}` is declared differently by two registered ports")]
    RecordConflict { record: String },

    #[error("the pack refused the definition: {message}")]
    Pack { message: String },
}

/// Every record a table knows, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Records(HashMap<String, Option<PortDef>>);

impl Records {
    /// Collects ports by record name, keeping `None` where two disagree.
    pub fn of<'a>(ports: impl Iterator<Item = &'a PortDef>) -> Self {
        let mut records = HashMap::new();
        for port in ports {
            match records.get(port.record.as_ref()) {
                Some(Some(known)) if !port.same_record(known) => {
                    records.insert(port.record.to_string(), None);
                }
                Some(_) => {}
                None => {
                    records.insert(port.record.to_string(), Some(port.clone()));
                }
            }
        }
        Self(records)
    }

    /// The record `record` names, as a port called `port`.
    pub fn port(&self, port: &str, record: &str) -> Result<PortDef, DefError> {
        match self.0.get(record) {
            Some(Some(def)) => Ok(PortDef {
                name: port.to_string().into(),
                ..def.clone()
            }),
            Some(None) => Err(DefError::RecordConflict {
                record: record.to_string(),
            }),
            None => Err(DefError::UnknownRecord {
                port: port.to_string(),
                record: record.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::utils::{Imu, Nav};
    use crate::{Output, Record};

    #[test]
    fn a_known_record_is_returned_under_the_port_name() {
        let records = Records::of([Output::<Imu>::def("imu")].iter());
        let port = records.port("plant.imu", Imu::NAME).expect("known record");
        assert_eq!(port.name, "plant.imu");
        assert_eq!(port.id, Output::<Imu>::def("imu").id);
    }

    #[test]
    fn an_unknown_record_names_the_port_that_wanted_it() {
        let records = Records::of([Output::<Imu>::def("imu")].iter());
        assert_eq!(
            records.port("out", Nav::NAME),
            Err(DefError::UnknownRecord {
                port: "out".into(),
                record: Nav::NAME.into(),
            })
        );
    }

    #[test]
    fn two_declarations_of_one_record_disagreeing_is_a_conflict() {
        let mut wide = Output::<Imu>::def("imu");
        wide.max_len += 8;
        let records = Records::of([Output::<Imu>::def("imu"), wide].iter());
        assert_eq!(
            records.port("out", Imu::NAME),
            Err(DefError::RecordConflict {
                record: Imu::NAME.into(),
            })
        );
    }

    #[test]
    fn the_empty_context_knows_nothing() {
        let cx = DefCx::empty();
        assert!(cx.inputs.is_empty() && cx.outputs.is_empty());
        assert!(cx.records.port("out", "imu").is_err());
    }
}
