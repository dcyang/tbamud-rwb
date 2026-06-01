# Telnet / Protocol Support in tbamud-rwb

Adapted from the stock TbaMUD "ProtocolSystem" document, which ships KaVir's
public-domain C protocol snippet (MSDP, GMCP/ATCP, MSSP, MXP, MCCP, MSP, plus
NAWS, TTYPE, CHARSET, and XTerm 256-color handling).

The Rust rewrite does **not** include the KaVir protocol suite. It implements
only the small amount of telnet negotiation needed for a clean line-mode session
and for hidden password entry. Everything else a modern client offers
(MSDP/GMCP/MXP/MCCP/MSSP/256-color) is currently unsupported; clients that
request those options are simply not answered, which is harmless.

## What the rewrite negotiates

(`src/telnet.rs`, `src/descriptor.rs`)

IAC command constants and option codes are defined in `src/telnet.rs`:

```
IAC WILL/WONT/DO/DONT, SB/SE, GA
options: ECHO (1), SUPPRESS-GA (3), NAWS (31), TTYPE (24)
```

Helpers and where they're used:

- `cmd_echo_off()` = `IAC WILL ECHO` — sent before a password prompt so the
  client stops local echo (the password isn't shown).
- `cmd_echo_on()` = `IAC WONT ECHO` — sent after the password to restore echo.
- `cmd_suppress_ga()` = `IAC WILL SUPPRESS-GA` — standard for interactive MUDs.
- `cmd_do_naws()` = `IAC DO NAWS` — asks the client for window size.
- `cmd_do_ttype()` = `IAC DO TTYPE` — asks the client for its terminal type.

Inbound, `strip_telnet()` removes IAC command sequences and subnegotiations
(`IAC SB ... IAC SE`) from raw socket input, returning only the printable
application bytes for the line interpreter. This means the server tolerates a
client sending NAWS/TTYPE/other subnegotiations — it just discards them rather
than acting on them. (So window size and terminal type are requested but not yet
consumed.)

## Color

Color is handled separately from the protocol layer, via the `@x` code system
(see [color.md](color.md)), rendered to ANSI SGR at send time. There is no XTerm
256-color or RGB support; the palette is the 16 ANSI colors.

## If you want to add advanced protocols

The clean place to hook MSDP/GMCP/MXP/MCCP would be the per-connection writer and
reader in `src/descriptor.rs`, with option-state tracking added alongside the
telnet constants in `src/telnet.rs`. Because the rewrite already isolates raw
socket I/O there (and strips/builds IAC sequences in one module), adding a
protocol handler is localized — but none of it exists today, and the stock goal
of player-facing parity does not require it (these are client-side enhancements,
not gameplay).
