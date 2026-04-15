//! rcheevos memaddr expression parser.
//!
//! Grammar covers size prefixes (`0xH`, `0x `, `0xW`, `0xX`, `0xL`, `0xU`,
//! `0xM`..`0xT`, bitcount, BCD, inverted, float variants) and modifiers
//! (`d` delta, `p` prior, `b` BCD, `~` inverted).
