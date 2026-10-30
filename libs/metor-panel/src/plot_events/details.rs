//! Shared event detail fields for plot popovers and inspector pages.
use super::EventDetail;

/// Semantic fields stay consistent across timeline and plot inspection.
pub(crate) fn fields(detail: &EventDetail) -> Vec<(String, String)> {
    match detail {
        EventDetail::Log(event) => {
            let mut fields = vec![
                ("level".into(), format!("{:?}", event.level)),
                ("source".into(), event.source.to_string()),
                ("message".into(), event.message.to_string()),
            ];
            fields.extend(
                event
                    .fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string())),
            );
            fields
        }
        EventDetail::Alarm(event) => vec![
            ("alarm".into(), event.def_id.clone()),
            ("severity".into(), format!("{:?}", event.severity)),
            ("detail".into(), event.detail.clone()),
        ],
        EventDetail::Sequence(entry) => vec![
            ("channel".into(), entry.channel.to_string()),
            (
                "state".into(),
                crate::sequences::state_label(entry.state).into(),
            ),
            ("message".into(), entry.message.to_string()),
        ],
        EventDetail::Raw(len) => vec![(
            "payload".into(),
            format!("{len} bytes (no schema announced)"),
        )],
        EventDetail::Json(_) => Vec::new(),
    }
}
