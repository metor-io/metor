# AGENTS.md / CLAUDE.md 

### What is metor-fsw

metor-fsw is a framework for developing flight software. It is modular in nature. This folder contains the third iteration of this that we are iterating on

### Style Guide

metor-fsw-3 is a piece of flight software so, we want to follow a relatively strict style guide. We will start with the way code is written:

- Code shall be short, concise, and straight-forward to read and understand. This means that long unwiedely functions are banned. Functions should have clear singular goals, and stick to them.
- Code shall be written in a data-first way. We should write core data structures in a way that is designed to maximize performance and functionality. Then we will write functions that operate on these data structures.
- Code shall be written to be composable. In line with the above edicts, we shall favor composability. If you are tempted to create mega structs or functions, attempt to compose the feature out of smaller reusable components.
- Comments shall be short, concise, and rare. Doc comments are good, but should only ever explain the current state of the code. It should also only explain the small bit of code you are looking at. Rememeber the code was written for a reason BUT it might be used for many other reasons. Comments are a form of failure; it means the code was not clear enough;
- Please remove all mannered prose from comments and docs
- Allocation is allowed, but only at initilization time. Care should be taken to not allocate memory after the software has reached steady state.
- Panics are generally not allowed, and should be avoided. There are cases where they are appropriate or unavoidable, like calling into libstd code, but unwrap should be avoided as much as possible. When there is a panic make sure to add a short "PANIC Safety" comment.
- We should endevour to write "good" clean Rust. Imagine you are an expert Rust programmer who has been using Rust for over 10 years. Follow all the standard ideomatic conventions

### Testing

Generally speaking we should strive for every piece of code we write to be tested, and designed to be easily testable. Broadly we should catagorized our tests into a few categories:
- Unit tests - These should be io-less, fast to run, and be colocated next to the code in a "tests" mod. Either in new file if the tests get long, or at the bottom of the file if the tests are short.
- Integration tests - These should be located outside of the crate in a tests crate, and are free to use IO.
- Prover tests - These should be located close to the code like unit tests. We should use of Kani and Verus where possible to prove the correctness of the code.

Generally all forms of test should at a minimum cover 3 cases: nominal / happy path, off-nominal / edge cases, and failure cases.

### Development Workflow

Generally we will follow a pattern to most our development:
1. We spawn an agent to write a doc about the feature we are working on.
2. We iterate on that doc, until the architecture is good, and the doc is clear
3. We ask the agent to write a detailed implementation plan about that feature
4. We iterate on the plan
5. We let the agent implement the plan
6. We do a form of antagonistic code review where another agent (usually a different model) reviews the code and provides feedback.
