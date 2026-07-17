"""Version marker for the metor-fsw pack ABI.

This distribution carries no code: its *version* is the `FSW_ABI_VERSION`
(`src/abi/mod.rs`) the depending artifacts were built against. Pack wheels
pin it exactly and `metor-fsw` depends on its own ABI version, turning an
ABI mismatch into a resolver conflict instead of a load failure
(`docs/design-packaging.md` §9.1).
"""
