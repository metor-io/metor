metor-panel is a UI for metor-db. It is built atop gpui. You can read about its current design in design.md. Please follow the style guide in style.md

Right now we are rolling our own reflection scheme for use with the command palette and property inspector, and its getting a little out of hand. There are three core problems:

### Nested State Management

Right now we do not have a great form of interior mutability for our types. This means that to modify a type we are manually implemeting path / patch semantics in the reflection scheme. Gross

Instead we should be using a core gpui primitive called Entity. Instead of owning sub-types we want to inspect ourselves, we would hold an Entity<T> and then inspect that directly. This makes sub-types more symetric with top-level types. It means that we can just pop open inspectors for sub-types and re-use all the existing code paths, with no difference. 

### Hand-rolled reflection is getting out of hand

The hand rolled reflection implementations are getting kinda crazy. They are a large part of the code we are writing, and are error-prone. Instead we should use `facet_reflect` to provide us reflection for free

### Serialization and reflecton asymetry

Reflection and serialization are basically the same problem. They ask, how can I take this data in my app, and turn it into strongly typed data outside of my app. The answer is we should use the same system for both serialization. We should use Facet for this

### How we integrate this

Ok we want to integrate facet with gpui and our existing systems. One problem is that we want to use Entity to provide interor mutability. To do this we need a two way trip between Facet type erased types and gpui type erased types

We can two maps: one between a TypeId and a function like:
```rust
fn(&'a AnyEntity, cx: gpui::Context) -> facet_reflect::Peek<'a, 'static>
```

and a second one back from facet ConstTypeId

```rust
fn(&'_ facet_reflect::Peek<'_, '_>) -> AnyEntity;
```

We probably also want the Poke equivalenents so we can get mutable data, maybe store each of those in a struct

That way we can take an Entity, and reflect it.

### Type Specific Widgets

We have some type specific widgets right now, for colors, booleans, strings, and components. These should stay, but their specific behavior will be encoded for a specific type. We might want another registry for this so you can register dynamic widgets for a specific type like:

```rust
HashMap<TypeId, Fn(AnyEntity) -> AnyElement>>
```

There is an open question about how we handle types where a pure element doesn't make sense. For instance, if you are selecting a Component, you want to see a list of components on the next page or in a side menu.

### Out of band state

Certain types like `ModelEntry` have out of band state that needs to be synced when a change is made. Generally we should have a core, relatively pure, inner data type, then we should have outer wrapper types that contain whatever state we need. We can then use `cx.subscribe` to handle sync between the state

### Registries

A pattern I want to expand here, is the ability to register global handlers in a registry. You can see some examples above. Generally the idea is that you can register a type, into this global register, and then the rest of the app can handle it. Right now this is used for our internal purposes, but in the future it will help us support user specific UI elements.

### General Notes

You should be free to access Facet at /Users/sphw/code/metor/facet. We probably want to implement the Facet types for gpui, you should feel free to do that.

This is a complex set of changes, please create a good multi-stage plan that will let you progressively implemenet the features. It will require refactoring large parts of the app. Do not be scared of changing how things work to make this happen. Just try and do it in an incremental fashion
