CREATE TABLE [app].[typed] (
    [id] int NOT NULL,
    [amount] decimal(18, 4) NULL,
    [body] nvarchar(max) NULL,
    [at] datetime2(3) NULL
);
ALTER TABLE [app].[typed] ADD [added] nvarchar(max) NULL;
DECLARE @df sysname, @definition nvarchar(max), @sql nvarchar(max);
SELECT @df = dc.name, @definition = dc.definition
FROM sys.default_constraints dc
JOIN sys.columns c ON c.object_id = dc.parent_object_id
AND c.column_id = dc.parent_column_id
WHERE dc.parent_object_id = OBJECT_ID(N'[app].[typed]') AND c.name = N'id';
IF @df IS NOT NULL AND @definition IS NULL
THROW 50000, 'Cannot preserve an unreadable default during a column type change', 1;
IF @df IS NOT NULL
BEGIN
SET @sql = N'ALTER TABLE [app].[typed] DROP CONSTRAINT ' + QUOTENAME(@df);
EXEC(@sql);
END;
ALTER TABLE [app].[typed] ALTER COLUMN [id] bigint NOT NULL;
IF @df IS NOT NULL
BEGIN
SET @sql = N'ALTER TABLE [app].[typed] ADD CONSTRAINT ' + QUOTENAME(@df) + N' DEFAULT ' + @definition + N' FOR [id];';
EXEC(@sql);
END;