metor-panel is a gui for metor-db. It is built around gpui and designed as a control system / console for industrial control and aerospace. You can read about the design in design.md

Right now a key component in metor-panel is the command palette. It works great for modyifing state, but it isn't the only UI that a user might want for modfying state. In particular we want a slightly different UI for the right click menu:

This is going to be inspired by the unified "everything palette" in RAD Debugger. The following quote from Ryan Fleury the Developer of RAD Debugger should elucidate what we are after:

> The previous 15 versions of RADDBG all had a command palette show up when you hit F1 (by default). I've always liked command palettes and found them preferable, in many cases, to hunting through a UI tree for something, when I roughly knew a string which described what I was trying to find.

> In the next version, this will be replaced by an "everything palette", which lists commands, but also functions, recently opened files, recently opened projects, settings (global, window, and per-tab, including custom ones defined by visualizers), and so on.


The good news is metor-panel already has the primitve we need to make two slightly different UIs, sourced from the exact same data: `Inspectable`.

What we want is a right click menu, that allows the user to edit all the inspectable of the items. Right now the command palette UI has discouraged us from making widgets for common tasks, but it would be useful to have some. For instance:
- Line width: This can be a slider rather than just a value
- Color: This could be a classic color picker / color slider widget
- Files: A file picker could be selected

I've attached an image of how RAD Debugger does this pattern

I would also like to take this as an opertunity to slightly rework how we do "child" management. Right now there are 3 places that we manage, child items:
- Traces
- Widgets
- Models

Each of these use slightly different UIs, we should unify the flow to the following:

- A top level child (i,e widgets, models, traces) section
 - New Child
- Child A
  - Properties
  - Delete
-  Child B

 
 I would like you to enter plan mode, read all of the existing code and style guides. Then develop a plan for how we will achieve this. Since this is a large set of features it should be implemented in a staged way 
