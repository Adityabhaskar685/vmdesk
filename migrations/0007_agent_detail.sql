-- Why the VM's last agent sync failed (spec section 10). Written with `agent` by each
-- sync, and cleared by one that succeeds.
ALTER TABLE status ADD COLUMN agent_detail TEXT;
