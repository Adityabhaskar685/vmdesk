-- `status` holds the latest result per VM only (spec section 13).
CREATE UNIQUE INDEX status_vm ON status (vm_id);
