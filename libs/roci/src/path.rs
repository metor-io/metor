use impeller2::types::ComponentId;
use impeller2_wkt::ComponentMetadata;

pub trait ComponentPath: Clone {
    fn to_name(&self) -> String;
    fn to_component_id(&self) -> ComponentId;
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

    fn to_component_id(&self) -> ComponentId {
        ComponentId::new("")
    }

    fn is_empty(&self) -> bool {
        true
    }
}

impl<C: ComponentPath> ComponentPath for &'_ C {
    fn to_name(&self) -> String {
        C::to_name(*self)
    }

    fn to_component_id(&self) -> ComponentId {
        C::to_component_id(*self)
    }
}

impl ComponentPath for String {
    fn to_name(&self) -> String {
        self.clone()
    }

    fn to_component_id(&self) -> ComponentId {
        ComponentId::new(self)
    }
}

impl ComponentPath for &'_ str {
    fn to_name(&self) -> String {
        self.to_string()
    }

    fn to_component_id(&self) -> ComponentId {
        ComponentId::new(self)
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

    fn to_component_id(&self) -> ComponentId {
        ComponentId::new(&self.to_name()) // TODO: we can do this without an alloc by chaining hash functions
    }
}
