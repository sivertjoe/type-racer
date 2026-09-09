# type-racer

A terminal-based multiplayer typing race game, built with Rust and [ratatui](https://ratatui.rs/).

Race against friends over the network to type a sentence as fast and accurately as possible, or play solo to practice.

## Game modes

- **Single Game** — one typing race against a random sentence. Fastest (and most accurate) typist wins.
- **Knockout** — a multi-round tournament. Losers are eliminated each round until two players remain, who then play a best-of-3 final.
  - *Host-paced*: the host starts each round manually.
  - *Auto*: rounds start automatically.

## Usage

```sh
# Play solo
cargo run

# Host a game (prints a join code)
cargo run -- host

# Host with a specific join code
cargo run -- host --code ABCD

# Join a hosted game on the same network (found automatically)
cargo run -- connect ABCD
```

Run `cargo run -- --help` (or `help <command>`) for the full list of options.

### Playing over the internet

The host needs to forward TCP port `7878` to their machine (router settings, or a service like [ngrok](https://ngrok.com)). Players then connect directly to the host's public IP instead of relying on LAN discovery:

```sh
cargo run -- connect ABCD --ip <host's public IP>
```
