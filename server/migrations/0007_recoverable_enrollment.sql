ALTER TABLE agent_install_tokens ADD COLUMN device_id TEXT;
ALTER TABLE agent_install_tokens ADD COLUMN computer_name TEXT;
ALTER TABLE agent_install_tokens ADD COLUMN redemption_key_hash TEXT;
ALTER TABLE agents ADD COLUMN pending_auth_token_hash TEXT;
