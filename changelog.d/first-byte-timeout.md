- **A silent provider no longer hangs the turn.** Hosted providers get a 120 s
  response-header timeout, and a stream that sends headers and then nothing fails with a
  first-byte timeout instead of waiting forever. A 429 whose body names a usage limit,
  quota or budget is reported as an exhausted plan limit, with when it resets, instead
  of being retried four times.
