//! Shared event detail fields for plot popovers and inspector pages.
use super::EventDetail;

/// Semantic fields stay consistent across timeline and plot inspection.
pub(crate) fn fields(detail: &EventDetail) -> Vec<(String, String)> {
    match detail {
        EventDetail::Log(event) => {
            let mut fields = vec![
                ("level".into(), format!("{:?}", event.level)),
                ("source".into(), event.source.clone()),
                ("message".into(), event.message.clone()),
            ];
            fields.extend(event.fields.iter().map(|(k, v)| (k.clone(), v.clone())));
            fields
        }
        EventDetail::Alarm(event) => vec![
            ("alarm".into(), event.def_id.clone()),
            ("severity".into(), format!("{:?}", event.severity)),
            ("detail".into(), event.detail.clone()),
        ],
        EventDetail::Sequence(event) => vec![
            ("channel".into(), event.channel_name.to_string()),
            ("event".into(), event.label.to_string()),
        ],
        EventDetail::Raw(len) => vec![(
            "payload".into(),
            format!("{len} bytes (no schema announced)"),
        )],
        EventDetail::Json(_) => Vec::new(),
    }
}
