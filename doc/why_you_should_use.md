# Why you should use Sōzu

- **Hot configurable:** Sozu can receive configuration changes at runtime through secure unix sockets.
- **Upgrades without restarting:** Sozu is always-up, meaning it upgrades itself *while still processing requests*.
- **Handles SSL:** Sozu handles SSL, so your backend servers can focus on what they do best.
- **Protects your network:** Sozu protect backends by shielding them behind the reverse proxy, limiting direct network access. Sozu uses Rust, a language primed for memory safety. And even if a worker is exploited, sozu workers are sandboxed.
