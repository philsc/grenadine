# TODO

- Check against a real two-turn `devin acp` run whether `cognition.ai/totalAcuCost`, and `agent_stopped`'s `acuCost`, which a turn's cost now comes from, are per-process or session-wide totals. `TurnSink` bills from 0 each turn, so a session-wide total would bill earlier turns again after `session/load`.
