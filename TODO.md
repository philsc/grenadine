# TODO

- Check against a real two-turn `devin acp` run whether `cognition.ai/totalAcuCost` is a per-process or a session-wide total. `TurnSink` bills from 0 each turn, so a session-wide total would bill earlier turns again after `session/load`.
