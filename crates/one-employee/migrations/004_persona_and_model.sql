-- one-employee 004: bind a persona (assistant) and an LLM model to a digital
-- employee, so runs stop being backend-only.
--
-- Until now `provision_run` created every conversation with `model: None` and
-- `assistant: None`, which made aionrs-backed employees fail 100% of the time
-- with `Provider '' not found` (the NULL model column resolves to the empty
-- sentinel in aionui_conversation::task_options::empty_provider_model, and the
-- aionrs factory then looks up provider id ""). Column semantics mirror
-- CronAgentConfigWriteDto, which already solved the same problem for cron jobs:
--
--   assistant_id      -- persona / assistant definition id. NULL keeps the
--                        legacy backend-only behaviour for existing rows.
--   agent_id_override -- agent_metadata.id to run the persona under, when the
--                        user manually overrode the backend the persona would
--                        otherwise imply. Forwarded as
--                        AssistantConversationOverridesRequest.agent_id (the
--                        only channel that works once `assistant` is set, since
--                        an assistant snapshot overrides CreateConversationRequest.type).
--                        Named after the existing assistant_states.agent_id_override.
--   model_id          -- plain model id, used by ACP backends via the assistant
--                        overrides (`extra.current_model_id` downstream).
--   model             -- ProviderWithModel JSON, used by aionrs as the top-level
--                        CreateConversationRequest.model. Top-level model is
--                        aionrs-only; other agent types get a hard 400.
--
-- `agent_type` (NOT NULL, from 001) is deliberately left in place: it keeps
-- storing the *effective* backend and stays the gate that decides whether a
-- top-level model may be sent at all.

ALTER TABLE one_personal_agents ADD COLUMN assistant_id TEXT;
ALTER TABLE one_personal_agents ADD COLUMN agent_id_override TEXT;
ALTER TABLE one_personal_agents ADD COLUMN model_id TEXT;
ALTER TABLE one_personal_agents ADD COLUMN model TEXT;
