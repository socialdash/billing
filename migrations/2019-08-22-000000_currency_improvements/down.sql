ALTER TABLE invoices DROP COLUMN currency;
ALTER TABLE invoices ADD COLUMN currency_id INTEGER NOT NULL DEFAULT 6;
