//! SQL Server's closed predicate policy (DEC-2400.20–47). No connection or execution.
use crate::Mssql;
use pbps_dialect::predicate::{CallInfo, CastType, Error, Form, Function, Result, Syntax};
use pbps_model::{ColumnType, TypeArg};

const fn f(name: &'static str, form: Form, arity: (usize, usize), major: u16) -> Function {
    Function {
        name,
        form,
        arity,
        major,
    }
}
const FUNCTIONS: &[Function] = &[
    f("CAST", Form::Cast, (1, 1), 10),
    f("CONVERT", Form::Convert, (2, 3), 10),
    f("COALESCE", Form::Ordinary, (2, 65536), 10),
    f("ISNULL", Form::Ordinary, (2, 2), 10),
    f("NULLIF", Form::Ordinary, (2, 2), 10),
    f("LEFT", Form::Ordinary, (2, 2), 10),
    f("RIGHT", Form::Ordinary, (2, 2), 10),
    f("SUBSTRING", Form::Ordinary, (2, 3), 10),
    f("CHARINDEX", Form::Ordinary, (2, 3), 10),
    f("REPLACE", Form::Ordinary, (3, 3), 10),
    f("LOWER", Form::Ordinary, (1, 1), 10),
    f("UPPER", Form::Ordinary, (1, 1), 10),
    f("LTRIM", Form::Ordinary, (1, 2), 10),
    f("RTRIM", Form::Ordinary, (1, 2), 10),
    f("TRIM", Form::Trim, (1, 2), 14),
    f("LEN", Form::Ordinary, (1, 1), 10),
    f("CONCAT", Form::Ordinary, (2, 254), 11),
    f("ROUND", Form::Ordinary, (2, 3), 10),
    f("FLOOR", Form::Ordinary, (1, 1), 10),
    f("CEILING", Form::Ordinary, (1, 1), 10),
    f("ABS", Form::Ordinary, (1, 1), 10),
    f("DATEADD", Form::Datepart, (3, 3), 10),
    f("DATEDIFF", Form::Datepart, (3, 3), 10),
    f("DATEPART", Form::Datepart, (2, 2), 10),
    f("DATEFROMPARTS", Form::Ordinary, (3, 3), 11),
    f("JSON_VALUE", Form::Ordinary, (2, 2), 13),
];
impl Syntax for Mssql {
    fn string_literal(&self, value: &str, national: bool) -> String {
        if national {
            crate::ident::literal(value)
        } else {
            format!("'{}'", value.replace('\'', "''"))
        }
    }
    fn function(&self, name: &str) -> Option<&'static Function> {
        FUNCTIONS.iter().find(|f| f.name.eq_ignore_ascii_case(name))
    }
    fn requirement(&self, function: &Function, call: CallInfo) -> (u16, u16) {
        match function.name {
            "TRIM" if call.directional => (16, 160),
            "LTRIM" | "RTRIM" if call.arity == 2 => (16, 160),
            "SUBSTRING" if call.arity == 2 => (17, 0),
            "JSON_VALUE" if !call.literal_path => (14, 0),
            _ => (function.major, 0),
        }
    }
    fn cast_type(&self, name: &str, args: Vec<TypeArg>) -> Result<CastType> {
        let mut ty = crate::types::normalize(&ColumnType::new(name, args.clone()))
            .map_err(|e| Error::new(e.to_string()))?;
        if args.is_empty()
            && matches!(
                ty.base.as_str(),
                "char" | "varchar" | "nchar" | "nvarchar" | "binary" | "varbinary"
            )
        {
            // CAST/CONVERT default to 30, unlike column declarations' 1.
            ty.args = vec![TypeArg::Int(30)];
        }
        if ty.base == "sysname" {
            ty = ColumnType::new("nvarchar", vec![TypeArg::Int(128)]);
        }
        let base = "bit tinyint smallint int bigint decimal smallmoney money real float \
                    date time smalldatetime datetime datetime2 datetimeoffset char varchar \
                    nchar nvarchar binary varbinary uniqueidentifier xml sql_variant"
            .split_ascii_whitespace()
            .find(|base| *base == ty.base)
            .ok_or_else(|| Error::new("CAST/CONVERT require an allowlisted built-in type"))?;
        Ok(CastType {
            base,
            args: ty.args,
        })
    }
    fn datepart(&self, function: &Function, name: &str) -> Option<&'static str> {
        let part = match name.to_ascii_lowercase().as_str() {
            "year" | "yy" | "yyyy" => "YEAR",
            "quarter" | "qq" | "q" => "QUARTER",
            "month" | "mm" | "m" => "MONTH",
            "dayofyear" | "dy" | "y" => "DAYOFYEAR",
            "day" | "dd" | "d" => "DAY",
            "week" | "wk" | "ww" => "WEEK",
            "weekday" | "dw" | "w" => "WEEKDAY",
            "hour" | "hh" => "HOUR",
            "minute" | "mi" | "n" => "MINUTE",
            "second" | "ss" | "s" => "SECOND",
            "millisecond" | "ms" => "MILLISECOND",
            "microsecond" | "mcs" => "MICROSECOND",
            "nanosecond" | "ns" => "NANOSECOND",
            "tzoffset" | "tz" if function.name == "DATEPART" => "TZOFFSET",
            "iso_week" | "isowk" | "isoww" if function.name == "DATEPART" => "ISO_WEEK",
            _ => return None,
        };
        Some(part)
    }
}

#[cfg(test)]
mod tests;
