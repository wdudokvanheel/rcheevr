# rcheevr

rcheevr is a [RetroAchievements](https://retroachievements.org) client library written in Rust. It parses achievement, leaderboard and rich presence definitions, evaluates them against emulator memory once per frame, and reports unlocks and scores to the RetroAchievements web API.

The reference implementation is [rcheevos](https://github.com/RetroAchievements/rcheevos), a C library. rcheevr reimplements the parts an emulator needs without any C in the build, so one crate compiles for native targets and for `wasm32-unknown-unknown`. I wrote it for my emulator suite (unreleased yet), which has a desktop frontend and a browser frontend and needed the same achievement code in both.

rcheevos is only used in the test suite. It is compiled from a git submodule and run next to the Rust evaluator on the same memory, frame by frame, to check that both give the same answer.

## Structure

```text
              patch JSON from the server
                         |
                     patch.rs
                         |
      +------------------+------------------+
      |                  |                  |
 condition.rs      leaderboard.rs     richpresence.rs      parsing
 memaddr.rs
      |                  |                  |
      +------------------+------------------+
                         |
                    evaluator.rs  <---- peek(address, size)
                         |              supplied by the emulator
                     client.rs
                    /         \
             events.rs       http.rs
           (to the UI)    (to the server)
```

| Module | Contents |
| --- | --- |
| `memaddr.rs` | Operand parser. Size prefixes for bits, nibbles, 8/16/24/32-bit reads in both byte orders, bit count, IEEE and MBF floats. Modifiers for delta, prior, BCD and inverted reads. |
| `condition.rs` | Condition and group parser. Flags, comparison and arithmetic operators, hit targets in both the `(N)` and the legacy `.N.` form. |
| `evaluator.rs` | The per-frame state machine for one achievement. |
| `leaderboard.rs` | Start/cancel/submit logic and value expressions, including the legacy `a*2_b*3` value syntax. |
| `richpresence.rs` | Script parser and renderer with all rcheevos number formats and the built-in macros. |
| `patch.rs` | Converts the server's `patch` response into parsed definitions. |
| `client.rs` | `AchievementRuntime`: login, game loading, the frame loop, the outgoing request queue. |
| `http.rs` | The `HttpTransport` trait with reqwest, browser fetch and mock implementations. |
| `api.rs` | serde types for the web API. |
| `snapshot.rs` | Reads sized values out of a flat memory dump. |
| `events.rs` | The event enum handed to the frontend, with a JSON form. |
| `hash.rs` | ROM hashing. |

The crate has three runtime dependencies: `serde`, `serde_json` and `md-5`. reqwest and the wasm-bindgen crates are optional and sit behind the `http-reqwest` and `http-fetch` features.

## The evaluator

Parsed definitions are immutable. All mutable state (hit counts, previous values, pause and reset flags) lives in a separate `AchievementState`, so one parsed achievement can be evaluated from several places and resetting it means replacing the state.

`eval_achievement` takes a definition, its state and a `peek(address, size) -> u32` closure. It returns `true` on the frame the achievement triggers. For each condition group it does the following:

1. Scan the `PauseIf` conditions. If one holds, the group is paused. Hit counts freeze, but previous-frame values are still updated so that delta reads are correct on the frame the pause ends.
2. Find the `ResetNextIf` conditions that hold and mark the condition each of them targets. The mark skips over modifier rows (`AddSource`, `SubSource`, `AddAddress`, `Remember`) and lands on the next row that does a comparison.
3. Walk the conditions in order, carrying a running sum for `AddSource`/`SubSource`, a pointer offset for `AddAddress`, the truth value of an `AndNext`/`OrNext` chain, a shared hit pool for `AddHits`/`SubHits`, and the `Remember` register.

A `ResetIf` that fires in any group clears the hit counts of the whole achievement. The achievement triggers when the core group and at least one alternative group are satisfied.

An achievement that is already true on the first frame it is evaluated does not trigger. rcheevos starts every achievement in a waiting state and only arms it after it has been false once, which stops achievements from firing on stale memory right after a game loads. rcheevr does the same, including the detail that hit counts are reset while waiting.

Values are typed. An operand is either an unsigned integer or a float, and a sum or comparison that mixes the two promotes to float the way rcheevos does.

## The runtime

`AchievementRuntime` owns the session, the loaded game and the queue of pending requests. It is generic over the HTTP transport.

```rust,ignore
let mut ra = AchievementRuntime::new(ReqwestTransport::new()?);
ra.login_token(user, token).await?;
ra.load_rom(&rom_bytes).await?;

loop {
    emulator.run_frame();

    ra.evaluate(|addr, size| emulator.peek(addr, size));
    ra.flush().await;

    for event in ra.drain_events() {
        ui.show(event);
    }
}
```

`load_rom` hashes the ROM, asks the server for the game ID, downloads the definitions and starts a session. The session response lists what the user has already unlocked, and those achievements are skipped during evaluation.

The frame step is split in two. `evaluate` is synchronous and does no I/O. It runs every achievement and leaderboard, renders rich presence, and puts unlocks and score submissions on a queue. `flush` is async and sends what is on the queue. The split is there so a frontend can keep the network off the thread that runs the frame loop: it calls `evaluate` inline and hands `flush` to its async runtime. `peek_from` does both in one call for frontends that don't need this.

An `Unlocked` event is emitted when the server has accepted the unlock, not when the conditions were met. A request that fails stays on the queue and is sent again on the next `flush`, and the failure is reported as a `ServerError` event.

Loading is lenient. A definition that fails to parse is skipped and recorded as a warning on the loaded game, and the other definitions still load. A rich presence script that fails to parse is replaced by an empty one.

For frontends that have the console's memory as a flat dump, or that sit behind an FFI boundary where passing a closure is awkward, `evaluate_from_snapshot` takes a byte slice and a base address. `snapshot.rs` does the address translation and byte assembly for every operand size.

### Transport

The transport trait has one method:

```rust,ignore
pub trait HttpTransport {
    fn post_form<'a>(
        &'a mut self,
        url: &'a str,
        form: &'a [(&'a str, &'a str)],
    ) -> TransportFuture<'a>;
}
```

Every RetroAchievements endpoint the client uses is a form POST to the same URL with a different `r` parameter, so this is all the runtime needs. `TransportFuture` is a boxed future that is `Send` on native targets and not on wasm, where the `web_sys` request types are `!Send` and there is one thread anyway.

There are three implementations: `ReqwestTransport`, `FetchTransport` for the browser, and `MockTransport`, which replays queued responses and records the calls it received.

## Testing

Parsing the format is the easy part. Most of the work went into matching rcheevos in the cases the documentation doesn't cover: what a hit count does while a group is paused, which condition a `ResetNextIf` lands on when modifier rows follow it, what a delta read returns on the first frame. An evaluator that gets one of these wrong still passes most tests, and then awards an achievement at the wrong moment in one game out of a few hundred.

So the test suite uses rcheevos as an oracle. The `oracle/` crate builds rcheevos v12.3.0 from a submodule with the `cc` crate and wraps `rc_trigger_t` and `rc_value_t`. It is only built when the `oracle` feature is on, and that feature exists only for tests.

| Suite | Tests | What it checks |
| --- | ---: | --- |
| Unit tests in `src/` | 274 | Parsers, evaluator behaviour per flag, formatting, runtime session handling against `MockTransport`. |
| `tests/differential.rs` | 132 | Scripted memory traces run through rcheevr and rcheevos. The trigger result has to match on every frame. Covers each flag alone and in combination with the others. |
| `tests/fixture_parse.rs` | 6 | Parses the real definitions for Super Mario Land, Pokémon Red and Kirby's Dream Land: 160 achievements, 7 leaderboards and 3 rich presence scripts. |
| `tests/mock_ra_server.rs` | 4 | The full client flow over real HTTP against a local axum server that imitates the API. |
| `tests/rom_harness.rs` | 9 | The runtime driven by a real Game Boy ROM running in the emulator core. |
| `tests/rom_differential.rs` | 18 | Both evaluators reading the same emulator memory while a ROM runs. |

The largest test in `rom_differential.rs` boots Pokémon Red, gets past the intro, and plays 60,000 frames of scripted input (walking and opening menus). On every frame it evaluates all 91 core achievements and the 3 leaderboard value expressions in both implementations. That is 5.64 million comparisons, and they all agree.

This test found a bug the scripted traces had missed. Leaderboard values written in the legacy multiplier syntax were off by the multiplier, and it only showed once a real game's timer started counting.

No test contacts the real RetroAchievements servers.

### Running the tests

The ROM tests use the Game Boy core from the emulator repository as a dev-dependency and expect it checked out next to this one at `../gameboy-emulator`. They skip themselves when `TEST_ROM_GB` is not set.

```text
git submodule update --init

cargo test                                   # unit tests and fixtures
cargo test --features http-reqwest           # adds the mock server tests
cargo test --features oracle                 # adds the differential tests, needs a C compiler

TEST_ROM_GB=/path/to/pokemon-red.gb \
  cargo test --release --features oracle,http-reqwest
```

The last command runs everything. The ROM differential suite takes about five minutes in a release build.

## Limitations

- `load_rom` hashes the whole file with MD5. That is the RetroAchievements rule for Game Boy, Game Boy Color and Game Boy Advance. Systems whose rule strips a header or reads a disc image are not implemented.
- Previous-frame values are stored per condition. rcheevos stores them per memory reference. A delta or prior read on the right-hand side of a comparison uses the value stored for the left-hand side, which is only correct when both sides read the same address (`0xH1234!=d0xH1234`, the usual way to write "this byte changed"). A delta read behind an `AddAddress` pointer is also tracked against the base address while its group is paused. The Pokémon Red differential run hits neither case.
- Rich presence is tested against expected strings from real scripts and not against rcheevos, because the oracle crate doesn't wrap the rcheevos rich presence evaluator yet.
- The evaluator handles the `Measured` and `Trigger` flags, but the runtime never emits the `ProgressUpdate`, `ChallengeShow` and `ChallengeHide` events that would let a frontend draw progress bars and challenge indicators.
- Failed requests are retried on the next `flush` with no backoff and no cap on the queue.
- Hardcore mode is a flag sent to the server. Enforcing its rules (no save states, no rewind) is left to the emulator.
- The differential ROM test covers one game on one system.

## License

AGPL-3.0. rcheevos is MIT licensed and is only pulled in as a test-time submodule.
