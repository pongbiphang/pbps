use super::{lex::Token, *};

// One Pratt walk handles parentheses without speculative re-parsing. The
// temporary sum disappears at each operator: scalar/condition mixing refuses.
// Boxing it keeps recursive frames bounded in unoptimised builds too.
enum Term {
    Value(Scalar),
    Test(Predicate),
}
impl Term {
    fn value(self: Box<Self>) -> Result<Scalar> {
        match *self {
            Self::Value(v) => Ok(v),
            Self::Test(_) => Err(Error::new("expected a scalar expression")),
        }
    }
    fn test(self: Box<Self>) -> Result<Predicate> {
        match *self {
            Self::Test(v) => Ok(v),
            Self::Value(_) => Err(Error::new("expected a condition, not a scalar")),
        }
    }
}
pub(super) struct Parser<'a, S> {
    tokens: Vec<(Token, usize)>,
    at: usize,
    syntax: &'a S,
    nesting: usize,
    cases: usize,
}
impl<'a, S: Syntax> Parser<'a, S> {
    pub(super) fn read(text: &str, syntax: &'a S) -> Result<Predicate> {
        let mut p = Self {
            tokens: lex::tokens(text, syntax.lexicon())?,
            at: 0,
            syntax,
            nesting: 0,
            cases: 0,
        };
        let result = p.expression(0)?.test()?;
        if p.at != p.tokens.len() {
            return Err(p.error("unexpected trailing token"));
        }
        Ok(result)
    }
    fn error(&self, text: &str) -> Error {
        Error::new(format!(
            "{text} at byte {}",
            self.tokens.get(self.at).map_or(0, |t| t.1)
        ))
    }
    fn is(&self, word: &str) -> bool {
        matches!(self.tokens.get(self.at), Some((Token::Word(w, false), _)) if w.eq_ignore_ascii_case(word))
            || matches!(self.tokens.get(self.at), Some((Token::Mark(w), _)) if *w == word)
    }
    fn eat(&mut self, word: &str) -> bool {
        if self.is(word) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, word: &str) -> Result<()> {
        if self.eat(word) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {word}")))
        }
    }
    fn word(&mut self) -> Result<String> {
        match self.tokens.get(self.at) {
            Some((Token::Word(w, false), _)) => {
                let w = w.clone();
                self.at += 1;
                Ok(w)
            }
            _ => Err(self.error("expected an unquoted keyword")),
        }
    }
    fn value(&mut self) -> Result<Scalar> {
        self.expression(41)?.value()
    }
    fn expression(&mut self, minimum: u8) -> Result<Box<Term>> {
        self.nesting += 1;
        if self.nesting > 256 {
            return Err(self.error("expression nesting exceeds 256"));
        }
        let mut left = if self.eat("(") {
            let v = self.expression(0)?;
            self.expect(")")?;
            v
        } else if self.eat("NOT") {
            Box::new(Term::Test(predicate(Condition::Not(Box::new(
                self.expression(30)?.test()?,
            )))?))
        } else if self.is("+") || self.is("-") {
            let op = if self.eat("+") {
                "+"
            } else {
                self.expect("-")?;
                "-"
            };
            Box::new(Term::Value(scalar(Value::Unary(
                op,
                Box::new(self.expression(51)?.value()?),
            ))?))
        } else {
            Box::new(Term::Value(self.primary()?))
        };
        loop {
            // SQL Server arithmetic, comparison, NOT, AND, OR precedence.
            // IN/BETWEEN/LIKE are complete search-condition atoms, including
            // BETWEEN's internal AND; a scalar is never a boolean operand.
            let (op, binding) = match self.tokens.get(self.at).map(|t| &t.0) {
                Some(Token::Mark(op @ ("*" | "/" | "%"))) => (*op, 60),
                Some(Token::Mark(op @ ("+" | "-"))) => (*op, 50),
                Some(Token::Mark(
                    op @ ("=" | "<" | ">" | "<=" | ">=" | "<>" | "!=" | "!<" | "!>"),
                )) => (*op, 40),
                _ if self.is("AND") => ("AND", 20),
                _ if self.is("OR") => ("OR", 10),
                _ if ["IS", "NOT", "IN", "BETWEEN", "LIKE"]
                    .iter()
                    .any(|w| self.is(w)) =>
                {
                    ("test", 40)
                }
                _ => break,
            };
            if binding < minimum {
                break;
            }
            if op == "test" {
                left = Box::new(Term::Test(self.test(left.value()?)?));
                continue;
            }
            self.at += 1;
            let right = self.expression(binding + 1)?;
            left = combine(op, binding, left, right)?;
        }
        self.nesting -= 1;
        Ok(left)
    }
    fn test(&mut self, v: Scalar) -> Result<Predicate> {
        if self.eat("IS") {
            let not = self.eat("NOT");
            self.expect("NULL")?;
            return predicate(Condition::Null(v, not));
        }
        let not = self.eat("NOT");
        let kind = if self.eat("IN") {
            self.expect("(")?;
            let list = self.list()?;
            self.expect(")")?;
            if list.is_empty() {
                return Err(self.error("IN requires a nonempty scalar list"));
            }
            Condition::In(v, list, not)
        } else if self.eat("BETWEEN") {
            let low = self.value()?;
            self.expect("AND")?;
            let high = self.value()?;
            Condition::Between(v, low, high, not)
        } else if self.eat("LIKE") {
            let pattern = self.value()?;
            let escape = if self.eat("ESCAPE") {
                Some(self.value()?)
            } else {
                None
            };
            Condition::Like(v, pattern, escape, not)
        } else {
            return Err(self.error("expected IN, BETWEEN or LIKE"));
        };
        predicate(kind)
    }
    fn primary(&mut self) -> Result<Scalar> {
        if self.eat("NULL") {
            return scalar(Value::Null);
        }
        if self.eat("CASE") {
            return self.case();
        }
        let token = self
            .tokens
            .get(self.at)
            .map(|t| t.0.clone())
            .ok_or_else(|| self.error("missing expression"))?;
        self.at += 1;
        scalar(match token {
            Token::Number(v) => Value::Number(v),
            Token::String(v, n) => Value::String(v, n),
            Token::Word(w, quoted) => {
                if !quoted && self.eat("(") {
                    let function = self
                        .syntax
                        .function(&w)
                        .ok_or_else(|| self.error("function is not allowlisted"))?;
                    let call = self.call(function)?;
                    self.expect(")")?;
                    let n = call.info().arity;
                    if n < function.arity.0 || n > function.arity.1 {
                        return Err(self.error(&format!("{} has invalid arity", function.name)));
                    }
                    Value::Call(function, call)
                } else {
                    if !quoted && (self.syntax.lexicon().reserved)(&w.to_ascii_lowercase()) {
                        return Err(self.error("reserved word is not a bare column"));
                    }
                    Value::Column(ColumnName::new(w))
                }
            }
            Token::Mark(_) => {
                return Err(self.error("expected a literal, column or allowlisted function"));
            }
        })
    }
    fn list(&mut self) -> Result<Vec<Scalar>> {
        let mut values = Vec::new();
        if !self.is(")") {
            loop {
                values.push(self.value()?);
                if !self.eat(",") {
                    break;
                }
            }
        }
        Ok(values)
    }
    fn call(&mut self, f: &Function) -> Result<Call> {
        Ok(match f.form {
            Form::Ordinary => Call::Ordinary(self.list()?),
            Form::Cast => {
                let v = self.value()?;
                self.expect("AS")?;
                Call::Cast(Box::new(v), self.ty()?)
            }
            Form::Convert => {
                let ty = self.ty()?;
                self.expect(",")?;
                let v = self.value()?;
                let style = if self.eat(",") {
                    let negative = self.eat("-");
                    if !negative {
                        self.eat("+");
                    }
                    let token = self.tokens.get(self.at).map(|t| &t.0);
                    let Some(Token::Number(n)) = token else {
                        return Err(self.error("CONVERT style must be an integer constant"));
                    };
                    let n = format!("{}{n}", if negative { "-" } else { "" })
                        .parse()
                        .map_err(|_| self.error("CONVERT style must be an i32 constant"))?;
                    self.at += 1;
                    Some(n)
                } else {
                    None
                };
                Call::Convert(ty, Box::new(v), style)
            }
            Form::Datepart => {
                let word = self.word()?;
                let part = self
                    .syntax
                    .datepart(f, &word)
                    .ok_or_else(|| self.error("unsupported datepart keyword"))?;
                self.expect(",")?;
                Call::Datepart(part, self.list()?)
            }
            Form::Trim => {
                let direction = ["LEADING", "TRAILING", "BOTH"]
                    .into_iter()
                    .find(|w| self.eat(w));
                let first = self.value()?;
                if self.eat("FROM") {
                    Call::Trim(direction, Some(Box::new(first)), Box::new(self.value()?))
                } else {
                    if direction.is_some() {
                        return Err(self.error("directional TRIM requires characters FROM value"));
                    }
                    Call::Trim(None, None, Box::new(first))
                }
            }
        })
    }
    fn ty(&mut self) -> Result<CastType> {
        let mut name = self.word()?;
        while matches!(self.tokens.get(self.at), Some((Token::Word(_, false), _))) && !self.is("AS")
        {
            name.push(' ');
            name.push_str(&self.word()?);
        }
        let mut args = Vec::new();
        if self.eat("(") {
            loop {
                if self.eat("MAX") {
                    args.push(TypeArg::Max);
                } else {
                    let Some((Token::Number(n), _)) = self.tokens.get(self.at) else {
                        return Err(self.error("type arguments must be integers or MAX"));
                    };
                    args.push(TypeArg::Int(
                        n.parse()
                            .map_err(|_| self.error("type arguments must be integers"))?,
                    ));
                    self.at += 1;
                }
                if !self.eat(",") {
                    break;
                }
            }
            self.expect(")")?;
        }
        self.syntax.cast_type(&name, args)
    }
    fn case(&mut self) -> Result<Scalar> {
        self.cases += 1;
        if self.cases > 10 {
            return Err(self.error("CASE nesting exceeds SQL Server's limit of 10"));
        }
        let input = if self.is("WHEN") {
            None
        } else {
            Some(self.value()?)
        };
        let mut simple = Vec::new();
        let mut searched = Vec::new();
        while self.eat("WHEN") {
            let test = self.expression(0)?;
            self.expect("THEN")?;
            let value = self.value()?;
            if input.is_some() {
                simple.push((test.value()?, value));
            } else {
                searched.push((test.test()?, value));
            }
        }
        if simple.is_empty() && searched.is_empty() {
            return Err(self.error("CASE requires a WHEN arm"));
        }
        let otherwise = if self.eat("ELSE") {
            Some(Box::new(self.value()?))
        } else {
            None
        };
        self.expect("END")?;
        self.cases -= 1;
        scalar(if let Some(v) = input {
            Value::SimpleCase(Box::new(v), simple, otherwise)
        } else {
            Value::SearchedCase(searched, otherwise)
        })
    }
}

fn combine(op: &'static str, binding: u8, left: Box<Term>, right: Box<Term>) -> Result<Box<Term>> {
    Ok(Box::new(if binding >= 50 {
        Term::Value(scalar(Value::Binary(
            op,
            Box::new(left.value()?),
            Box::new(right.value()?),
        ))?)
    } else if binding == 40 {
        let op = match op {
            "!=" => "<>",
            "!<" => ">=",
            "!>" => "<=",
            op => op,
        };
        Term::Test(predicate(Condition::Compare(
            op,
            left.value()?,
            right.value()?,
        ))?)
    } else {
        Term::Test(predicate(Condition::Logic(
            op,
            Box::new(left.test()?),
            Box::new(right.test()?),
        ))?)
    }))
}
