use metor_proto::hash::PathHasher;
use metor_proto::types::ComponentId;
use metor_proto_wkt::ComponentMetadata;

pub trait ComponentPath: Clone {
    fn to_name(&self) -> String;

    /// Feeds this path's segments into `hasher` in order.
    ///
    /// This is the alloc-free basis for [`to_component_id`](ComponentPath::to_component_id):
    /// a [`PathHasher`] rolls the same fnv1a-64 hash as `ComponentId::new(&self.to_name())`
    /// but without materializing the dotted string.
    fn hash_into(&self, hasher: &mut PathHasher);

    fn to_component_id(&self) -> ComponentId {
        let mut hasher = PathHasher::new();
        self.hash_into(&mut hasher);
        hasher.finish()
    }
    fn chain<B>(&self, other: B) -> ChainPath<Self, B>
    where
        Self: Sized,
    {
        ChainPath::new(self.clone(), other)
    }
    fn to_metadata(&self) -> ComponentMetadata {
        ComponentMetadata {
            component_id: self.to_component_id(),
            name: self.to_name(),
            metadata: Default::default(),
        }
    }
    fn is_empty(&self) -> bool {
        false
    }
}

impl ComponentPath for () {
    fn to_name(&self) -> String {
        String::new()
    }

    fn hash_into(&self, _hasher: &mut PathHasher) {}

    fn is_empty(&self) -> bool {
        true
    }
}

impl<C: ComponentPath> ComponentPath for &'_ C {
    fn to_name(&self) -> String {
        C::to_name(*self)
    }

    fn hash_into(&self, hasher: &mut PathHasher) {
        C::hash_into(*self, hasher)
    }
}

impl ComponentPath for String {
    fn to_name(&self) -> String {
        self.clone()
    }

    fn hash_into(&self, hasher: &mut PathHasher) {
        hasher.push(self);
    }
}

impl ComponentPath for &'_ str {
    fn to_name(&self) -> String {
        self.to_string()
    }

    fn hash_into(&self, hasher: &mut PathHasher) {
        hasher.push(self);
    }
}

#[derive(Clone)]
pub struct ChainPath<A, B> {
    a: A,
    b: B,
}

impl<A, B> ChainPath<A, B> {
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A, B> ComponentPath for ChainPath<A, B>
where
    A: ComponentPath,
    B: ComponentPath,
{
    fn to_name(&self) -> String {
        let a = self.a.to_name();
        let b = self.b.to_name();
        match (a.is_empty(), b.is_empty()) {
            (true, true) => String::new(),
            (true, false) => b,
            (false, true) => a,
            (false, false) => format!("{}.{}", a, b),
        }
    }

    fn hash_into(&self, hasher: &mut PathHasher) {
        // Chaining the two halves into the rolling hasher yields the same id as
        // `ComponentId::new(&self.to_name())`, but without the intermediate `String`.
        self.a.hash_into(hasher);
        self.b.hash_into(hasher);
    }
}
