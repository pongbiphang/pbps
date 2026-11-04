# Schema

1 table(s), 2 column(s), 0 module(s).

## Diagram

```mermaid
erDiagram
    t_app_s_customer["app.customer"] {
        app.ordinary ordinary "ordinary"
        computed ordinary
    }
```

## Tables

### `app.customer`

| Column | Type | Null | Default | Description |
|---|---|---|---|---|
| `ordinary` ~~deprecated~~ | `app.ordinary` | yes | `ordinary` | ordinary |

| Computed column | Expression | Stored |
|---|---|---|
| `ordinary` | `ordinary` | persisted |

## Extensions

| Name | Schema | Exact version |
|---|---|---|
| `ordinary` | `ordinary` | `ordinary` |

## Types

### `app`


#### `app.domain_value`

*Domain*

| Property | Value |
|---|---|
| Base type | `app.ordinary` |
| NOT NULL | no |
| Default | `ordinary` |
| Collation | `app.ordinary` |

| Check | Expression |
|---|---|
| `ordinary` | `ordinary` |

#### `app.enum_value`

*Enum*

| Label |
|---|
| `ordinary` |

## Roles

### `reporter`

| Granted on | Permissions |
|---|---|
| `app.ordinary` | select |

## Do not use

These columns still exist but are on their way out.

| Column | Reason |
|---|---|
| `app.customer.ordinary` | ordinary |

## Graveyard

Dropped objects, kept so an audit can be answered.

| Was | Dropped | By | Reason |
|---|---|---|---|
| `ordinary` | ordinary | ordinary | ordinary |
