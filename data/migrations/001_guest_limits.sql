-- Guests are counted per worksheet, so five an hour and five in all ran out after one
-- workbook or a handful of small files. Only plans still at the first limits are changed.
UPDATE plans SET imports_per_hour = 30, max_collections = 25
 WHERE id = 'guest' AND imports_per_hour = 5 AND max_collections = 5;
