CREATE TABLE sessions (
    id TEXT PRIMARY KEY, source TEXT, model TEXT, title TEXT,
    started_at REAL, ended_at REAL, parent_session_id TEXT,
    input_tokens INTEGER, output_tokens INTEGER
);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT,
    tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL
);
INSERT INTO sessions VALUES ('alice-hermes', 'cli', 'synthetic-model', 'Synthetic checks', 1767225600, 1767225610, NULL, 10, 2);
INSERT INTO messages VALUES
(1, 'alice-hermes', 'assistant', 'Checking.', NULL,
 '[{"id":"error","name":"read_file","arguments":{"path":"/home/alice/project/file.txt"}},{"id":"success","name":"search_files","arguments":{}},{"id":"exit","name":"terminal","arguments":{}},{"id":"nested","name":"read_file","arguments":{}},{"id":"empty","name":"read_file","arguments":{}},{"id":"plain","name":"read_file","arguments":{}},{"id":"refused","name":"read_file","arguments":{}},{"id":"landed","name":"write_file","arguments":{}}]', NULL, 1767225601),
(2, 'alice-hermes', 'tool', '{"exit_code":2,"output":"Synthetic failure."}', 'exit', NULL, 'terminal', 1767225602),
(3, 'alice-hermes', 'tool', '{"error":"Synthetic missing file."}', 'error', NULL, 'read_file', 1767225603),
(4, 'alice-hermes', 'tool', '{"success":false}', 'success', NULL, 'search_files', 1767225604),
(5, 'alice-hermes', 'tool', '{"result":{"error":"Ordinary payload."},"success":true}', 'nested', NULL, 'read_file', 1767225605),
(6, 'alice-hermes', 'tool', '{"error":"","exit_code":0,"success":true}', 'empty', NULL, 'read_file', 1767225606),
(7, 'alice-hermes', 'tool', 'Error is a word in this synthetic document.', 'plain', NULL, 'read_file', 1767225607),
(8, 'alice-hermes', 'tool', '{"guardrail_refusal":true,"error":"Synthetic refusal."}', 'refused', NULL, 'read_file', 1767225608),
(9, 'alice-hermes', 'tool', '{"bytes_written":12,"success":false}', 'landed', NULL, 'write_file', 1767225609);
