# Metro Panel

Metor Panel is a platform for creating UIs for control systems and realtime telemetry. It provides a frontend for metor-db, a time series database designed for telemetry. Think of it like LabView, but you define the UI in code using a scripting language (JS or Python).

## Metro DB
In metor DB each piece of telemetry is an N dimensional tensor called a component. We store components in time series arrays, that are compatible with arrow. Metor DB allows you to query the database with either SQL or a bespoke RPC system.  

## Data Flow
A core component of metor-panel is data-flow. The user defines a series of transformations on data, called Operations. These operations take in a "component" and produce a new component. In metor-db, components are made up of a WAL log, and a time series array. Operations primarimly take place on the WAL log. The WAL log is based on a data-type called the "distruptor". Which is like a broadcast ring buffer.

Operations can aggregate data with an internal buffer. Take for example an operation like chunk, which takes N samples, and returns a tensor with shape [N, ...]. You could cohain this operation with a mean operation that would take each of the samples in the chunk, and calculate the mean.

A natural question arrises, how do we handle operations on multiple components? For example if we wanted to add two components together? What if those two components come from diverging time-domains. We could call the add function when either component has a new value, using the latest value from each component. We could wait till both have a new value. We could only add when a single component has a new value. We could add either at a fixed rate. We could downsample one component at the data rate of the other.
The truth is these are all valid and legitment options depending on the situation. So, we let the user decide which of these options to choose. They can define a new time-domain, and then select how to sample this. See the below pseudo-code:

```rust
    let clock = Clock::hz(100):
    let a = last_value(signal_a, clock);
    let b = last_value(signal_b, clock);
    let c = a + b;
```

In some cases you don't actually want a full disruptor for each value. For instance if you want to express: `signal_a * 2.0 + 1.0`, you don't want to have to allocate a new buffer for each part of this expression. Instead you want to be able to lazily evaluate these expressions. To do this we generalize the disruptor into a trait like:
```rust
trait ComponentStream {
    type View<'_>;
    async fn next(&mut self) -> Self::View<'_>; 
}

pub trait AsComponentView {
    fn as_component_view(&self) -> ComponentView<'_>;
}
impl AsComponentView for Component {
    // impl left as an exercise for the reader
}
impl AsComponentView for ComponentView {
    // impl left as an exercise for the reader
}
```

## Architecture

metor-panel is built atop [gpui](https://www.gpui.rs). gpui is based on the concept of Elements. metor-panel is essentially a way of mapping `ComponentStream` into gpui Elements. 

There are a set of pre-defined elements for common use cases, for instance a plot element that takes a `ComponentStream` of values and renders them as a line plot. Or a simple text element that takes a ComponentStream of values and renders them as text. Generally users define layouts of elements using scripts built with JS running inside QuickJS. 

Many elements inside metor-panel implement a thin form of reflection that allows them to be configured and inspected at runtime. The following trait is used:
```rust
trait Inspectable {
    fn items(&self) -> Vec<InspectionItem>;
}

struct InspectionField {
    title: String,
    field_id: FieldId,
    value: InspectionValue,
}

enum InspectionValue{
   Color { r: f32, g: f32, b: f32, a: f32 },
   Component { name: String },
   // Other items
}
```

Inspectable elements can be inspected at runtime via the command palette, or via an element inspector UI.
