use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn grid(self, x: i32, y: i32) -> usize {
        let third = |point, start, length: i32| {
            (((i64::from(point) - i64::from(start) + 1) * 3 - 1) / i64::from(length.max(1)))
                .clamp(0, 2) as usize
        };
        third(y, self.y, self.height) * 3 + third(x, self.x, self.width)
    }
}

#[derive(Clone, Debug)]
struct Expr(Vec<(f64, usize)>);

impl FromStr for Expr {
    type Err = String;
    fn from_str(input: &str) -> Result<Self, String> {
        let text = input.trim().to_ascii_lowercase();
        let mut rest = text.as_str();
        let mut terms = Vec::new();
        while !rest.is_empty() {
            let sign = if let Some(more) = rest.strip_prefix('-') {
                rest = more;
                -1.0
            } else if let Some(more) = rest.strip_prefix('+') {
                rest = more;
                1.0
            } else if terms.is_empty() {
                1.0
            } else {
                return Err(input.into());
            };
            let count = rest
                .bytes()
                .take_while(|byte| byte.is_ascii_digit() || *byte == b'.')
                .count();
            let number = &rest[..count];
            rest = &rest[count..];
            let unit = ["sw", "sh", "ww", "wh", "vw", "vh"]
                .iter()
                .position(|unit| rest.starts_with(unit));
            let factor = if number.is_empty() && unit.is_some() {
                100.0
            } else {
                number.parse::<f64>().map_err(|_| input.to_string())?
            };
            if !factor.is_finite() {
                return Err(input.into());
            }
            if unit.is_some() {
                rest = &rest[2..];
            }
            terms.push((factor * sign, unit.map_or(0, |index| index + 1)));
        }
        if terms.is_empty() {
            return Err("empty expression / 空表达式".into());
        }
        Ok(Self(terms))
    }
}

#[derive(Clone, Debug)]
pub struct Layout {
    fields: [Expr; 4],
    pub monitor: u32,
    virtual_origin: bool,
}

impl FromStr for Layout {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        let mut parts = text.split('|');
        let mut next = || parts.next().ok_or_else(|| text.to_owned())?.parse::<Expr>();
        let fields = [next()?, next()?, next()?, next()?];
        let monitor = parts
            .next()
            .map(|part| part.trim().parse())
            .transpose()
            .map_err(|_| text.to_owned())?
            .unwrap_or(0);
        if parts.next().is_some() {
            return Err(text.into());
        }
        let virtual_origin = fields
            .iter()
            .any(|expr| expr.0.iter().any(|(_, unit)| *unit >= 5));
        Ok(Self {
            fields,
            monitor,
            virtual_origin,
        })
    }
}

impl Layout {
    pub fn resolve(&self, screen: Rect, window: Rect, desktop: Rect) -> Result<Rect, String> {
        let origin = if self.virtual_origin { desktop } else { screen };
        let factors = [
            100,
            screen.width,
            screen.height,
            window.width,
            window.height,
            desktop.width,
            desktop.height,
        ];
        let eval = |index: usize, offset: i32| {
            let value = self.fields[index]
                .0
                .iter()
                .map(|(factor, unit)| factor * f64::from(factors[*unit]) / 100.0)
                .sum::<f64>()
                .round()
                + f64::from(offset);
            if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value) {
                return Err("coordinate overflow / 坐标越界".to_owned());
            }
            Ok(value as i32)
        };
        Ok(Rect {
            x: eval(0, origin.x)?,
            y: eval(1, origin.y)?,
            width: eval(2, 0)?,
            height: eval(3, 0)?,
        })
    }
}
