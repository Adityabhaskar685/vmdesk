-- One agent per VM: a re-imaged VM's new agent replaces the old row (spec section 10).
CREATE UNIQUE INDEX agents_vm ON agents (vm_id);
