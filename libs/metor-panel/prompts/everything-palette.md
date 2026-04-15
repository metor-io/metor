metor-panel is a gpui based gui for metor-db. It currently has a series of features based on Facet reflection that allow the user to dynamically modify things on screen. Right now there is a sort of side channel that uses PalletePage to funnel an old command palette system into this reflection based system. I think there is some room for improvement here.

One of the goals of the inspector and associated command palette system is to be an "everything palette" a unified UI where the user can find and edit any aspect of the application. 
  
## Items Registry 

In order to support this, we want to create a new registry that allows us to store all the inspectable entities in the application. Roughly the data type should be something like:
```rust
struct ItemRegistry {
    items: HashMap<Catagory, Vec<InspectionItem>>
    entities: HashMap<EntityId, AnyEntity>
}

enum Catagory {
    Panel,
    Widget,
    Command,
    Custom,
}

enum InspectionItem {
    Entity(EntityId),
    Command(Command)
}
```

Then when we open the command palette we would display a top level inspector that shows all of the items. The user can then click on an item to inspect it, or on a command to run it. This would replace the current command palette system located in `command_palette`


Can you enter plan mode, and make a plan for how to implement this. Please read the existing codebase carefully. Also if you have any questions or decisions please explore multiple options and present them to me.
