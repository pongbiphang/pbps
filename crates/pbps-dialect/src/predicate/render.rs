use super::*;

pub(super) fn condition<S: Syntax>(p: &Predicate, s: &S) -> Result<String> {
    let text = match &p.kind {
        Condition::Compare(op, a, b) => format!("{} {op} {}", value(a, s)?, value(b, s)?),
        Condition::Null(v, not) => format!("{} IS {}NULL", value(v, s)?, neg(*not)),
        Condition::In(v, list, not) => {
            format!("{} {}IN ({})", value(v, s)?, neg(*not), values(list, s)?)
        }
        Condition::Between(v, low, high, not) => format!(
            "{} {}BETWEEN {} AND {}",
            value(v, s)?,
            neg(*not),
            value(low, s)?,
            value(high, s)?
        ),
        Condition::Like(v, pattern, escape, not) => format!(
            "{} {}LIKE {}{}",
            value(v, s)?,
            neg(*not),
            value(pattern, s)?,
            escape
                .as_ref()
                .map(|e| value(e, s).map(|v| format!(" ESCAPE {v}")))
                .transpose()?
                .unwrap_or_default()
        ),
        Condition::Not(v) => format!("NOT {}", condition(v, s)?),
        Condition::Logic(op, a, b) => format!("{} {op} {}", condition(a, s)?, condition(b, s)?),
    };
    Ok(format!("({text})"))
}
fn neg(not: bool) -> &'static str {
    if not { "NOT " } else { "" }
}
fn values<S: Syntax>(list: &[Scalar], s: &S) -> Result<String> {
    Ok(list
        .iter()
        .map(|v| value(v, s))
        .collect::<Result<Vec<_>>>()?
        .join(", "))
}
fn ty(ty: &CastType) -> String {
    if ty.args.is_empty() {
        ty.base.to_ascii_uppercase()
    } else {
        format!(
            "{}({})",
            ty.base.to_ascii_uppercase(),
            ty.args
                .iter()
                .map(|a| a.raw().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
fn value<S: Syntax>(v: &Scalar, s: &S) -> Result<String> {
    Ok(match &v.kind {
        Value::Number(n) => n.clone(),
        Value::String(v, n) => s.string_literal(v, *n),
        Value::Null => "NULL".into(),
        Value::Column(n) => s
            .quote_ident(n.raw())
            .map_err(|e| Error::new(e.to_string()))?,
        Value::Unary(op, v) => format!("({op} {})", value(v, s)?),
        Value::Binary(op, a, b) => format!("({} {op} {})", value(a, s)?, value(b, s)?),
        Value::Call(f, call) => {
            let args = match call {
                Call::Ordinary(v) => values(v, s)?,
                Call::Cast(v, t) => format!("{} AS {}", value(v, s)?, ty(t)),
                Call::Convert(t, v, style) => format!(
                    "{}, {}{}",
                    ty(t),
                    value(v, s)?,
                    style.map(|n| format!(", {n}")).unwrap_or_default()
                ),
                Call::Datepart(p, v) => format!("{p}, {}", values(v, s)?),
                Call::Trim(direction, chars, v) => format!(
                    "{}{}{}",
                    direction.map(|d| format!("{d} ")).unwrap_or_default(),
                    chars
                        .as_ref()
                        .map(|c| value(c, s).map(|c| format!("{c} FROM ")))
                        .transpose()?
                        .unwrap_or_default(),
                    value(v, s)?
                ),
            };
            format!("{}({args})", f.name)
        }
        Value::SimpleCase(input, arms, otherwise) => {
            let mut text = format!("CASE {}", value(input, s)?);
            for (a, b) in arms {
                text.push_str(&format!(" WHEN {} THEN {}", value(a, s)?, value(b, s)?));
            }
            finish_case(text, otherwise, s)?
        }
        Value::SearchedCase(arms, otherwise) => {
            let mut text = "CASE".to_owned();
            for (a, b) in arms {
                text.push_str(&format!(" WHEN {} THEN {}", condition(a, s)?, value(b, s)?));
            }
            finish_case(text, otherwise, s)?
        }
    })
}
fn finish_case<S: Syntax>(
    mut text: String,
    otherwise: &Option<Box<Scalar>>,
    s: &S,
) -> Result<String> {
    if let Some(v) = otherwise {
        text.push_str(&format!(" ELSE {}", value(v, s)?));
    }
    text.push_str(" END");
    Ok(format!("({text})"))
}
