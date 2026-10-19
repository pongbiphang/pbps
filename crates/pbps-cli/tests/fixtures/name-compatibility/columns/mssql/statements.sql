CREATE TABLE [app].[child] (
    [col
line[31m\‮名前] int NOT NULL,
    [col
line[31m\‮名前payload] int NOT NULL CONSTRAINT [DF_pbps_child_col
line[31m\‮名前payload] DEFAULT (2
),
    [col
line[31m\‮名前computed] AS (1 + 1
) PERSISTED,
    CONSTRAINT [pk_child] PRIMARY KEY CLUSTERED ([col
line[31m\‮名前])
);
ALTER TABLE [app].[child] ADD CONSTRAINT [uq_child] UNIQUE ([col
line[31m\‮名前payload]);
ALTER TABLE [app].[child] ADD CONSTRAINT [fk_parent] FOREIGN KEY ([col
line[31m\‮名前payload]) REFERENCES [app].[parent] ([col
line[31m\‮名前]);
CREATE INDEX [ix_child] ON [app].[child] ([col
line[31m\‮名前] ASC) INCLUDE ([col
line[31m\‮名前payload]);
CREATE TABLE [app].[parent] (
    [col
line[31m\‮名前] int NOT NULL
);
CREATE TABLE [app].[temporal] (
    [col
line[31m\‮名前start] datetime2(7) GENERATED ALWAYS AS ROW START NOT NULL,
    [col
line[31m\‮名前end] datetime2(7) GENERATED ALWAYS AS ROW END NOT NULL,
    PERIOD FOR SYSTEM_TIME ([col
line[31m\‮名前start], [col
line[31m\‮名前end])
);
BEGIN TRANSACTION;
BEGIN TRY
DECLARE @pbps_key_before nvarchar(max);
UPDATE [app].[child] SET @pbps_key_before = CONVERT(nvarchar(max), [col
line[31m\‮名前]), [col
line[31m\‮名前payload] = 2 WHERE [col
line[31m\‮名前] = N'1' AND CONVERT(nvarchar(max), [col
line[31m\‮名前payload]) = N'1' COLLATE Latin1_General_BIN2;
IF @@ROWCOUNT <> 1 THROW 50000, N'app.child row `1` is not as the plan recorded it: changed or deleted since the plan was made. Plan again.', 1;
IF NOT EXISTS (SELECT 1 FROM [app].[child] WHERE [col
line[31m\‮名前] = N'1' AND CONVERT(nvarchar(max), [col
line[31m\‮名前]) = @pbps_key_before COLLATE Latin1_General_BIN2 AND DATALENGTH(CONVERT(nvarchar(max), [col
line[31m\‮名前])) = DATALENGTH(@pbps_key_before) AND CONVERT(nvarchar(max), [col
line[31m\‮名前payload]) = N'2' COLLATE Latin1_General_BIN2)
  THROW 50000, N'app.child row `1` is not what this plan wrote once the statement had run — a trigger on the table, another writer inside it, or a value the engine stores differently from the way it is declared. Nothing was applied; `pbps plan --db` says which.', 1;
COMMIT TRANSACTION;
END TRY
BEGIN CATCH
IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;
THROW;
END CATCH