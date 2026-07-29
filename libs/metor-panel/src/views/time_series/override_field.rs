/// An inspector-friendly replacement for `Option<T>` used by plot override
/// fields. `Auto` means "fall back to the auto-computed behavior"; `Custom(v)`
/// pins the value.
#[derive(Clone, Debug, Default, PartialEq, facet::Facet, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum Override<T> {
    #[default]
    Auto,
    Custom(T),
}

impl<T: facet::Facet<'static> + 'static> Override<T> {
    pub fn as_custom(&self) -> Option<&T> {
        match self {
            Self::Auto => None,
            Self::Custom(v) => Some(v),
        }
    }
}

impl<T> Override<T> {
    /// Mirror of [`Option::map`]. `Auto` stays `Auto`; `Custom(v)` becomes
    /// `Custom(f(v))`.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Override<U> {
        match self {
            Self::Auto => Override::Auto,
            Self::Custom(v) => Override::Custom(f(v)),
        }
    }

    /// Mirror of [`Option::as_ref`]: borrow the inner value through the
    /// override wrapper.
    pub fn as_ref(&self) -> Override<&T> {
        match self {
            Self::Auto => Override::Auto,
            Self::Custom(v) => Override::Custom(v),
        }
    }
}
