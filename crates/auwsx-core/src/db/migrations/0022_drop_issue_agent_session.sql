-- Agents run as daemon-owned subprocesses now; tmux session tracking is legacy.
ALTER TABLE issues DROP COLUMN agent_session;
