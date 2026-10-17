ALTER TABLE [app].[name
line[31m\‮名前] ADD [name
line[31m\‮名前1] int NULL;
DECLARE @df sysname = (
    SELECT dc.name FROM sys.default_constraints dc
      JOIN sys.columns c ON c.object_id = dc.parent_object_id
                        AND c.column_id = dc.parent_column_id
     WHERE dc.parent_object_id = OBJECT_ID(N'[app].[name
line[31m\‮名前]') AND c.name = N'name
line[31m\‮名前2');
IF @df IS NOT NULL
BEGIN
    DECLARE @sql nvarchar(max) = N'ALTER TABLE [app].[name
line[31m\‮名前] DROP CONSTRAINT ' + QUOTENAME(@df);
    EXEC(@sql);
END
ALTER TABLE [app].[name
line[31m\‮名前] DROP COLUMN [name
line[31m\‮名前2];
ALTER TABLE [app].[name
line[31m\‮名前] ALTER COLUMN [name
line[31m\‮名前1] int NOT NULL;