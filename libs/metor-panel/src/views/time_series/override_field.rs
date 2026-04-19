/// An inspector-friendly replacement for `Option<T>` used by plot override
/// fields. `Auto` means "fall back to the auto-computed behavior"; `Custom(v)`
/// pins the value.
#[derive(Clone, Debug, PartialEq, facet::Facet)]
#[repr(u8)]
pub enum Override<T> {
    Auto,
    Custom(T),
}

impl<T: facet::Facet<'static> + 'static> Default for Override<T> {
    fn default() -> Self {
        Self::Auto
    }
}

impl<T: facet::Facet<'static> + 'static> Override<T> {
    pub fn as_custom(&self) -> Option<&T> {
        match self {
            Self::Auto => None,
            Self::Custom(v) => Some(v),
        }
    }
}
