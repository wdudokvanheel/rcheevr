# rveevr Testing Methodology & Compliance

This document describes how **rveevr** — a pure-Rust RetroAchievements evaluator — is tested, how compliance with the reference C implementation (rcheevos) is verified, and what known weaknesses and potential problems remain.

## Table of Contents

- [Goals & Constraints](#goals--constraints)
- [Architecture Overview](#architecture-overview)
- [Testing Layers](#testing-layers)
  - [Layer 1: Unit Tests](#layer-1-unit-tests)
  - [Layer 2: Differential Testing vs rcheevos](#layer-2-differential-testing-vs-rcheevos)
  - [Layer 3: Fixture-Based Parsing](#layer-3-fixture-based-parsing)
  - [Layer 4: Mock Server Integration](#layer-4-mock-server-integration)
  - [Layer 5: ROM Integration Harness](#layer-5-rom-integration-harness)
  - [Layer 6: ROM Differential (Large-Scale)](#layer-6-rom-differential-large-scale)
- [The Oracle: FFI Bridge to rcheevos](#the-oracle-ffi-bridge-to-rcheevos)
- [Compliance Strategy](#compliance-strategy)
- [Known Weaknesses](#known-weaknesses)
- [Potential Problems](#potential-problems)
- [What Is Not Tested](#what-is-not-tested)
- [Running the Test Suite](#running-the-test-suite)

---

## Goals & Constraints

rveevr reimplements the rcheevos evaluator in Rust with two hard constraints:

1. **Zero FFI in production** — the crate must compile to native desktop and WebAssembly targets from a single codebase. C dependencies are only used during testing.
2. **Zero production-account risk** — no automated test ever contacts the real RetroAchievements servers or uses real credentials. All network interactions are mocked.

The testing strategy follows from these constraints: since we cannot link rcheevos in production and cannot test against live RA, we must build offline confidence that the Rust evaluator is bit-for-bit identical to the C one.

---

## Architecture Overview

The crate is structured as:

| Module | Responsibility |
|--------|---------------|
| `memaddr.rs` | Parse rcheevos memory address expressions into operand AST |
| `condition.rs` | Parse condition strings into flag/operand/op/hit-count structs; split condsets |
| `evaluator.rs` | Per-frame state machine: evaluate conditions, track hits, handle flags |
| `richpresence.rs` | Parse and evaluate rich presence scripts (Format, Lookup, Display sections) |
| `leaderboard.rs` | Leaderboard state machine (START/CANCEL/SUBMIT/VALUE) and value formatting |
| `patch.rs` | Deserialize RA patch endpoint JSON into loaded game structures |
| `api.rs` | Serde types for all RA API endpoints |
| `client.rs` | Top-level `AchievementRuntime<H: HttpTransport>` coordinating everything |
| `http.rs` | `HttpTransport` trait with reqwest and mock implementations |
| `hash.rs` | ROM hashing (MD5, per-system rules) |

The **oracle** crate (`oracle/`) compiles rcheevos from C source and exposes it through a safe Rust wrapper. It is feature-gated behind `--features oracle` and never included in production builds.

---

## Testing Layers

### Layer 1: Unit Tests

**Location:** Inline `#[test]` blocks in `src/*.rs`

**Coverage:**
- **`api.rs`** (~11 tests): JSON deserialization of every RA endpoint response type. Validates serde rename attributes, optional fields, the `#[serde(flatten)]` envelope pattern, and error propagation.
- **`condition.rs`** (~20 tests): Flag parsing for all 14 condition flag types. Operand parsing across all memory size prefixes. Hit count syntax (legacy `.N.` and modern `(N)`). Condset splitting — critically, distinguishing the `S` condset separator from the `0xS` (Bit6) size prefix.
- **`client.rs`** (~15 tests): Session lifecycle, game loading error cases, rich presence caching, event queue semantics, hardcore mode toggling.
- **`memaddr.rs`**: Operand parser coverage for every `MemSize` variant (Byte, Word, DWord, BigEndian, nibbles, individual bits, BitCount, Float, MBF32, Double32), modifier prefixes (`d` delta, `p` prior, `b` BCD, `~` inverted), and immediate operands.
- **`leaderboard.rs`**: Value format display for all `ValueFormat` variants (TimeSecs, Frames, Score, Fixed1/2/3, etc.).

**What this catches:** Parser regressions, serde deserialization breakage, basic API contract violations. These tests are fast (~2s total) and run on every `cargo test` invocation without special flags.

---

### Layer 2: Differential Testing vs rcheevos

**Location:** `tests/differential.rs` (~800 LOC, 50+ test cases)
**Requires:** `--features oracle` (C toolchain)

This is the core compliance mechanism. Each test constructs a synthetic memory trace and feeds it frame-by-frame to both rveevr and rcheevos, asserting they produce identical triggered/not-triggered results on every frame.

**Test harness:**

```
For each frame 0..N:
    step(frame, &mut memory)           // User-defined memory mutation
    rveevr_result  = rust_eval(memory)  // Our evaluator
    rcheevos_result = c_eval(memory)    // Reference via FFI
    assert_eq!(rveevr_result, rcheevos_result)
```

**Coverage spans all 14 condition flags and their interactions:**

| Category | Example Tests |
|----------|--------------|
| Basic comparison | `simple_equality_trigger`, `not_equal`, `greater_than`, `less_than` |
| Hit counts | `hit_count_latches`, `hit_count_exact_frame` |
| PauseIf | `pauseif_blocks_hits`, `pauseif_in_alt_doesnt_freeze_core` |
| ResetIf | `resetif_clears_all_hits`, `resetif_with_hit_count` |
| ResetNextIf | `resetnextif_skips_modifiers`, `resetnextif_through_addsource` |
| AddSource/SubSource | `addsource_chain`, `subsource_negative_result` |
| AddHits/SubHits | `addhits_shared_pool`, `subhits_partial_subtract` |
| AndNext/OrNext | `and_next_both_true_triggers`, `or_next_either_fires` |
| Measured/MeasuredIf | `measured_tracks_progress`, `measuredif_gates_display` |
| Trigger flag | `trigger_delays_until_fired`, `trigger_priming` |
| AddAddress | `indirect_address_read`, `addaddress_chain` |
| Remember/Recall | `remember_stores_recall_reads` |
| Multi-condset | `core_plus_two_alts`, `resetif_in_alt_resets_globally` |
| Delta/Prior | `delta_detects_change`, `prior_tracks_last_different` |
| WAITING state | `waiting_blocks_trivial_boot_trigger` |

**Why synthetic traces matter:** Synthetic memory gives us full control over exactly when conditions become true, which frames trigger resets, and how accumulators interact. This is essential for testing edge cases that would be impractical to reproduce with a real ROM.

---

### Layer 3: Fixture-Based Parsing

**Location:** `tests/fixture_parse.rs` (~200 LOC)
**Fixtures:** `tests/fixtures/*.json` (captured RA patch responses)

These tests parse real achievement definitions from production RA games and verify the parser handles them without errors. The fixtures contain actual patch endpoint JSON for:

- **Super Mario Land** — 34 core achievements
- **Pokemon Red** — 91 core achievements
- **Kirby's Dream Land** — 35 core achievements

**What this catches:** Parser gaps when confronted with real-world achievement strings. RA achievement authors use the full breadth of the rcheevos language, including obscure size prefixes, deeply nested AddSource chains, and complex hit count patterns that synthetic tests might not cover.

**Failure mode:** Tests accumulate all parse failures and report them at once rather than short-circuiting, so a single run shows the full picture of parser compatibility.

---

### Layer 4: Mock Server Integration

**Location:** `tests/mock_ra_server.rs` (~365 LOC)

An axum-based HTTP server spawned on an ephemeral port simulates the RA web API. Tests exercise the full client flow:

1. `login2` — password and token authentication
2. `gameid` — ROM hash to game ID lookup
3. `patch` — fetch achievement definitions
4. `startsession` — begin session, receive unlock state
5. `awardachievement` — submit unlocks
6. `submitlbentry` — submit leaderboard scores
7. `ping` — session keepalive

**Test cases:**
- **Full unlock flow:** Login, load ROM, evaluate frames until achievement triggers, verify `awardachievement` HTTP call was made with correct parameters.
- **Login failure propagation:** Wrong password returns structured error.
- **Unknown game handling:** GameID=0 for unrecognized ROMs.
- **Ping timing:** Session keepalive fires after the expected frame count.

**What this catches:** Client-level bugs in session management, event queuing, API request construction, and response parsing that unit tests on individual modules would miss.

---

### Layer 5: ROM Integration Harness

**Location:** `tests/rom_harness.rs` (~443 LOC)
**Requires:** `$TEST_ROM_GB` environment variable

Runs the rveevr evaluator against a real Game Boy ROM with a mock transport backend. This tests the full vertical slice: ROM hashing, achievement parsing from JSON, per-frame evaluation with real emulator memory, and event generation.

**Key tests:**
- **ROM header constants:** Known byte at address 0x0104 (Nintendo logo) triggers achievement
- **WRAM initialization detection:** Achievement on `0xHC001>0` fires when game writes to work RAM
- **Delta detection:** `d0xHC001!=0xHC001` catches the moment memory changes
- **Staggered hit counts:** Two achievements on same address with hit targets of 10 and 60 fire on exactly the right frames
- **No re-award:** Once an achievement fires, it does not fire again in the same session
- **Rich presence rendering:** RP script produces expected display string
- **Hash determinism:** Same ROM always produces the same MD5

**What this catches:** Integration bugs between the emulator's memory bus and the evaluator's peek callback, as well as any assumptions the evaluator makes about memory layout that don't hold on real hardware.

---

### Layer 6: ROM Differential (Large-Scale)

**Location:** `tests/rom_differential.rs` (~876 LOC)
**Requires:** `--features oracle` + `$TEST_ROM_GB`

This is the most comprehensive test. It boots a real Pokemon Red ROM, plays through 60,000 frames (~17 minutes of gameplay) with scripted inputs, and evaluates **every achievement and leaderboard** through both rveevr and rcheevos on every frame.

**Setup:**
- Boots ROM and skips the intro sequence deterministically
- Schedules 50 walk cycles (Down/Right/Up/Left at 60 frames each)
- Schedules 10 menu interaction bursts (Start, navigate, A, B to close) spaced across the run

**Scale:**
- 91 core achievements evaluated per frame = **5,460,000 achievement evaluations**
- 3 leaderboard VALUE expressions per frame = **180,000 value evaluations**
- **Total: 5,640,000 evaluations compared bit-for-bit**

**Result:** Zero disagreements. Every triggered state and every leaderboard value matches between rveevr and rcheevos across the entire run.

**What this catches:** Subtle accumulator drift, state machine divergence under sustained evaluation, and edge cases that only emerge from the interaction of many achievements evaluating against the same evolving memory state. This is where we caught the legacy value format bug — leaderboard speedrun timers with multipliers (`0xhda45*1_0xhda44*60_...`) were producing values offset by exactly the multiplier amount, which was only visible after thousands of frames of accumulation.

---

## The Oracle: FFI Bridge to rcheevos

The `oracle/` subcrate compiles the rcheevos C library from vendored source (git submodule) and exposes it through safe Rust wrappers.

**Build process** (`oracle/build.rs`):
- Compiles 14 C translation units via the `cc` crate
- Links a static `librcheevos.a`
- Feature-gated: only built when `--features oracle` is passed

**FFI wrapper:**
- `oracle::Trigger` — wraps `rc_trigger_t` (opaque C struct)
  - `Trigger::parse(memaddr)` — allocates buffer, calls `rc_parse_trigger`
  - `Trigger::evaluate(peek_fn)` — calls `rc_evaluate_trigger` with a thunked Rust closure as the peek callback
- `oracle::Value` — wraps `rc_value_t` for leaderboard VALUE expressions
  - Same parse/evaluate pattern

**Callback thunking:** The Rust closure is packed into a `*mut c_void` user-data pointer and unpacked in an `extern "C"` thunk function, allowing rcheevos to call back into Rust for memory reads.

**Safety:** The oracle is never linked into production builds. It exists solely to provide a ground-truth reference during development and CI.

---

## Compliance Strategy

Compliance with rcheevos is established through a layered approach:

### 1. Specification-Driven Implementation

The evaluator was built by studying rcheevos source code and the [RADocs achievement creation guide](https://docs.retroachievements.org/developer-docs/). Every condition flag, comparison operator, memory size prefix, and operand modifier was implemented to match the documented semantics.

### 2. Differential Verification

The oracle bridge allows us to run both implementations side-by-side and assert identical outputs. This catches semantic divergences that would be invisible to unit tests alone. The differential suite covers:

- Every individual condition flag in isolation
- Pairwise interactions between flags (e.g., ResetNextIf through AddSource chains)
- Multi-condset behavior (core + alts, cross-condset reset propagation)
- State machine transitions (WAITING, Trigger priming, PauseIf blocking)
- Operand modifiers (delta, prior, BCD, inverted, float)

### 3. Real-World Achievement Parsing

Fixture tests against real RA game data (160+ achievements across 3 games) verify that the parser handles the full diversity of achievement definitions found in production.

### 4. Large-Scale Evaluation

The 60,000-frame Pokemon Red differential test subjects the evaluator to sustained real-world conditions. This is the test that found the legacy value format multiplier bug — a class of error that only manifests under continuous evaluation against real game memory.

### 5. Lenient Patch Loading

When loading game data from the RA API, unparseable achievements and leaderboards are logged and skipped rather than causing a hard failure. This ensures that if a game uses a condition flag or syntax we haven't encountered yet, the rest of the game's achievements still work. Rich presence parse errors do fail the load, since RP is expected for every game.

---

## Known Weaknesses

### 1. RHS Delta/Prior on Different Addresses

**Affected:** Conditions like `d0xH0001 > 0xH0002` where the delta operand is on a different address than the RHS.

**Behavior:** rveevr tracks delta/prior state per-condition, not per-address. When the RHS references a different address with a delta modifier, rveevr uses the LHS condition's state for both sides. rcheevos tracks modified memrefs globally per-address, so it would use the correct per-address delta for the RHS.

**Impact:** Low. The vast majority of delta comparisons compare an address against itself (`d0xH0000 != 0xH0000` — "did this byte change?"). Cross-address delta comparisons are rare in production achievements. The differential tests have not found a disagreement caused by this, but it is a theoretical divergence.

**Mitigation path:** Implement a global memref table keyed by (address, size) that tracks delta/prior values, matching rcheevos' `rc_memref_t` list. This would increase memory usage but eliminate the approximation.

### 2. SubHits Edge Cases

**Affected:** Complex `AddHits`/`SubHits` chains where multiple SubHits rows contribute to a shared hit pool alongside Standard rows.

**Behavior:** SubHits is implemented at the hit-pool level, subtracting its hit count from the shared accumulator. The interaction with combined AddHits + Standard rows in deeply nested chains may not match rcheevos exactly in edge cases.

**Impact:** Low to moderate. SubHits is a relatively uncommon flag. Simple cases (single SubHits reducing a hit pool) are covered by differential tests and pass. Edge cases with multiple SubHits rows interleaved with AddHits have not been extensively tested.

**Mitigation path:** Add more differential test cases specifically targeting SubHits chains with multiple contributors.

### 3. AddAddress + Delta Interaction

**Affected:** Conditions that use indirect addressing (`AddAddress`) combined with delta operands.

**Behavior:** When evaluating delta values for conditions modified by AddAddress, rveevr peeks the base address rather than the indirected address. rcheevos handles this through its modified memref system, which tracks deltas for the final resolved address.

**Impact:** Very low. AddAddress + delta is extremely rare in production achievements. The combination requires reading a previous frame's value at an address that is itself determined by another memory read, which is an unusual construction.

**Mitigation path:** Track delta values for resolved (post-AddAddress) addresses. This requires changes to the evaluation loop to resolve indirect addresses before updating delta state.

### 4. Single-Game Differential Coverage

**Affected:** Overall confidence in compliance.

**Behavior:** The large-scale ROM differential test currently covers only Pokemon Red (Game Boy). Different consoles and games exercise different subsets of the achievement language.

**Impact:** Moderate. While the 91 achievements in Pokemon Red cover a good breadth of condition flags and patterns, games on other platforms (SNES, GBA, N64) may use features or combinations not present in this game's achievement set.

**Mitigation path:** Add ROM differential tests for additional games, prioritizing games known to use complex features (leaderboards with value chains, rich presence with conditional display, Trigger flag usage).

### 5. No Negative Differential Tests

**Affected:** Detection of false positives.

**Behavior:** The differential tests verify that both evaluators agree. They do not specifically test that achievements *don't* trigger when they shouldn't, beyond what naturally falls out of the frame-by-frame comparison.

**Impact:** Low. The frame-by-frame comparison inherently catches false positives — if rveevr triggers on a frame where rcheevos doesn't, the test fails. However, there are no dedicated tests that construct memory states specifically designed to be "almost but not quite" satisfying.

**Mitigation path:** Add targeted differential tests with near-miss memory patterns for each flag type.

---

## Potential Problems

### 1. rcheevos Version Drift

**Risk:** The vendored rcheevos submodule represents a snapshot in time. As the upstream rcheevos project adds new features or changes semantics, rveevr may fall behind.

**Example scenarios:**
- A new condition flag is added (like the `G:` flag found in unofficial achievements, which we currently skip).
- The semantics of an existing flag are refined in a way that changes evaluation results.
- New memory size prefixes or operand modifiers are introduced.

**Mitigation:** Periodically update the vendored rcheevos submodule, re-run the differential suite, and implement any new features. Monitor the rcheevos changelog and RA developer forums for breaking changes.

### 2. Unofficial Achievements with Unknown Features

**Risk:** Unofficial achievements (flags != 3) may use experimental or undocumented features that rveevr cannot parse.

**Current behavior:** Unparseable achievements are skipped with a log warning. This is safe but means users won't see those achievements.

**Example:** Achievement 108362 (Pokemon Red, unofficial) uses a `G:` flag that rveevr does not recognize. This was encountered during real API testing.

**Impact:** Limited to unofficial achievement sets. Core (official) achievements go through a review process that generally keeps them within the documented feature set.

### 3. Rich Presence Complexity

**Risk:** Rich presence scripts can contain complex conditional display logic with nested lookups and format chains. The parser handles the standard Format/Lookup/Display structure, but the RA ecosystem allows custom extensions and conditional display lines that may have undocumented behaviors.

**Current coverage:** RP parsing is tested against real Tetris and Pokemon Red scripts. The FormatType parser accepts both `=` and `:` separators (a real-world variation discovered during testing). However, there is no differential testing for rich presence output — only rveevr's parser is tested, not compared frame-by-frame against rcheevos' RP evaluator.

**Mitigation path:** Extend the oracle to expose `rc_richpresence_t` evaluation and add differential RP tests.

### 4. Float Comparison Edge Cases

**Risk:** IEEE 754 floating point comparison has well-known edge cases (NaN, negative zero, denormals). rveevr uses epsilon-tolerant comparison matching rcheevos, but float-based achievements are uncommon and may not be well-represented in the differential test suite.

**Impact:** Low for Game Boy (no float-based achievements). Higher for platforms like PS1/N64 where float memory reads are more common.

### 5. Network Retry and Rate Limiting

**Risk:** The HTTP client has no exponential backoff or rate limiting yet. Award and leaderboard submit failures are queued and retried on later frames, but ping failures are surfaced as `ServerError`, and transient login/load failures still return errors.

**Impact:** In practice, RA API calls are infrequent (login once, patch once per game, achievements on unlock, ping every ~2 minutes). But repeated transient failures can still delay unlock reports or produce noisy runtime events.

**Mitigation path:** Add bounded exponential backoff with jitter for retryable operations (`awardachievement`, leaderboard submit, `ping`) and cap the pending queue if network failures persist.

### 6. Memory Bus Parity

**Risk:** The evaluator reads memory through a `peek(address, num_bytes) -> u32` callback provided by the emulator. If the emulator's memory map doesn't match what rcheevos expects for a given console, achievements will evaluate against wrong data.

**This is outside rveevr's scope** — it depends on the emulator's bus implementation. But it is a real integration risk. For example:
- Memory bank switching (GBC) must present the correct bank at the expected address range.
- Memory-mapped I/O registers may need to return specific values.
- Echo RAM regions should mirror their source.

**Current coverage:** The ROM differential test validates that the Game Boy emulator's `bus.peek()` produces the same values when called from rveevr as when called from rcheevos through the oracle's peek thunk. This is implicitly tested because both evaluators receive the same peek function and must agree on results.

### 7. Hash Algorithm Divergence

**Risk:** If rveevr's ROM hashing doesn't match rcheevos' per-console hashing rules, the wrong game ID will be returned from the RA API, and achievements for a different game (or no game) will be loaded.

**Current implementation:** GB hashing is a straightforward MD5 of the full ROM. Other consoles have more complex rules (header stripping, interleaving, etc.). Each new console integration needs its hash algorithm verified against rcheevos.

**Coverage:** Hash determinism is tested (same ROM always produces same hash), and the hash is implicitly validated when the mock server returns the expected game ID. Real API testing confirmed the hash matches for tested ROMs.

---

## What Is Not Tested

1. **Real RA server interaction** — by design, no automated test hits production. Manual testing on a throwaway account is the only way to verify end-to-end with real RA infrastructure.

2. **Rich presence differential** — RP output is tested for correctness in isolation but not compared frame-by-frame against rcheevos' RP evaluator.

3. **Leaderboard submission flow** — leaderboard VALUE evaluation is differentially tested, but the full submit → rank response flow is only tested against the mock server.

4. **Multiple console targets** — ROM differential tests currently cover Game Boy only. GBA, SNES, and other platforms are not covered.

5. **Concurrent session handling** — the RA API allows only one active session per user. rveevr does not test behavior when another client starts a session while we're active.

6. **Hardcore mode server-side enforcement** — hardcore mode is a client-side flag sent with API calls. We test that the flag is sent correctly, but server-side validation of hardcore submissions is not testable offline.

---

## Running the Test Suite

```bash
# Layer 1 + 3 + 4 + 5 (no C toolchain needed)
cargo test

# All layers including differential (requires C compiler)
cargo test --features oracle

# ROM-based tests (requires ROM file)
TEST_ROM_GB=/path/to/pokemon_red.gb cargo test --features oracle

# Specific differential test with output
TEST_ROM_GB=/path/to/pokemon_red.gb cargo test diff_pokemon_red_gameplay \
    --features oracle -- --nocapture

# CI configuration
cargo test -p rveevr                    # Always: layers 1, 3, 4, 5
cargo test -p rveevr --features oracle  # Linux runners with C toolchain
```
