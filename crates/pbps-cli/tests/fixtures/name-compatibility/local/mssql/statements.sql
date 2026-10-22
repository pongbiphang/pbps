CREATE TABLE [app].[child] (
    [id] int NOT NULL,
    [code] int NOT NULL,
    CONSTRAINT [local
line[31m\‮名前pk] PRIMARY KEY NONCLUSTERED ([id])
);
ALTER TABLE [app].[child] ADD CONSTRAINT [local
line[31m\‮名前uq] UNIQUE CLUSTERED ([code]);
ALTER TABLE [app].[child] ADD CONSTRAINT [local
line[31m\‮名前ck] CHECK (code > 0
);
ALTER TABLE [app].[child] ADD CONSTRAINT [local
line[31m\‮名前fk] FOREIGN KEY ([code]) REFERENCES [app].[parent] ([id]);
CREATE INDEX [local
line[31m\‮名前ix] ON [app].[child] ([code] ASC) INCLUDE ([id]) WHERE (code > 0
);
ALTER TABLE [app].[child] ADD CONSTRAINT [local
line[31m\‮名前pk] PRIMARY KEY CLUSTERED ([id]);
ALTER TABLE [app].[child] ADD CONSTRAINT [local
line[31m\‮名前uq] UNIQUE ([code]);
ALTER TABLE [app].[child] DROP CONSTRAINT [local
line[31m\‮名前uq];
ALTER TABLE [app].[child] ADD CONSTRAINT [local
line[31m\‮名前fk] FOREIGN KEY ([code]) REFERENCES [app].[parent] ([id]);
ALTER TABLE [app].[child] DROP CONSTRAINT [local
line[31m\‮名前fk];
ALTER TABLE [app].[child] ADD CONSTRAINT [local
line[31m\‮名前ck] CHECK (code > 0
);
ALTER TABLE [app].[child] DROP CONSTRAINT [local
line[31m\‮名前ck];
CREATE INDEX [local
line[31m\‮名前ix] ON [app].[child] ([code] ASC) INCLUDE ([id]) WHERE (code > 0
);
DROP INDEX [local
line[31m\‮名前ix] ON [app].[child];