I'd like to design a new feature in metor-panel, dynamic components. The core concept is user defined components through a node based edtiing gui. This is complicated feature so we should break it into two main parts:

## Dynamic Components

The core idea here is to spawn new tasks that are responsible for generating dynamic components. Broadly these can be broken up into a few main categories:
1. Generators – Components that are self-generating. Think of a sin wave or a random number generator.
2. Single component derivations – This could be things like multiplication, addition, or a some combination of these.
3. Re-sampling / Re-clocking – Often times we will want to handle components that are of different sample rates. We might want to resample them to a common sample rate. You can do this a number of ways: zero-order holds on the input, linear interpolation, or sampling the latest value at a given rate. The core idea here is that you can resample a component from one sample rate to another, and that sample rate can be another component's sample rate.
4. We want some way of composing these components together for instance adding two components together or getting their mean. 
5. We want to be able to persist these components as actual time series values

All in all, what we want is a toolkit of functions with a singature like:
```rust
fn sin(clock: Clock, freq: f64, amplitude: f64, phase: f64) -> impl ComponentStream
fn clock(component: Component) -> Clock
fn persist(component_stream: impl ComponentStream) -> TimeSeries
fn downsample(component_stream: impl ComponentStream, clock: f64) -> impl ComponentStream
``` 


## Node Editor

We also want to be able to construct these new components at runtime, using a node editor. This should be a runtime system with a few features:
1. We should base the UI off of: /Users/sphw/code/os/gpui-flow, but with our own spin on the theme
2. We should store the components in a graph data structure of some sort, and be able to identify each component by a hash of its arguments
3. There should be a degree of typing to the arguments for each node, so sin takes in a Clock, so it can only be connected to a Clock component
4. We should be able to persist the graph data structure like we can presets for dashboards and everything else
5. We should utilize the inspector for adding new nodes to the graph
6. When we modify the node we should detect changes to the nodes and dynamically spawn / despawn the new components

----

To start make a plan for the just the dynamic components, read the exisiting code, and plan to make most changes in a new module. Please ask for any help or ask clarifying questions. In your implementation consider how the node editor will need to work, but don't start planning it
