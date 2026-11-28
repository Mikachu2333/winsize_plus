use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, PartialEq)]
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
            let factor = round_decimal(factor);
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
fn round_decimal(value: f64) -> f64 {
    if value.abs() <= f64::MAX / 100.0 {
        (value * 100.0).round() / 100.0
    } else {
        value
    }
}
impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (factor, unit)) in self.0.iter().enumerate() {
            if index > 0 && *factor >= 0.0 {
                write!(f, "+")?;
            }
            let value = if *factor == 0.0 { 0.0 } else { *factor };
            let number = format!("{value:.2}");
            write!(
                f,
                "{}{}",
                number.trim_end_matches('0').trim_end_matches('.'),
                ["", "sw", "sh", "ww", "wh", "vw", "vh"][*unit]
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    fields: [Expr; 4],
    pub monitor: u32,
    virtual_origin: bool,
}
impl fmt::Display for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}|{}|{}|{}",
            self.fields[0], self.fields[1], self.fields[2], self.fields[3]
        )?;
        if self.monitor != 0 {
            write!(f, "|{}", self.monitor)?;
        }
        Ok(())
    }
}
/// Only numeric layout terms are normalized; names containing digits are untouched.
pub fn normalize(input: &str) -> Result<String, String> {
    input.parse::<Layout>().map(|layout| layout.to_string())
}

impl FromStr for Layout {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        let mut parts = text.split(['|', '｜', '、']);
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
    pub fn uses_virtual_desktop(&self) -> bool {
        self.virtual_origin
    }
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
                + f64::from(offset);
            if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value) {
                return Err("coordinate overflow / 坐标越界".to_owned());
            }
            Ok(value)
        };
        let x = eval(0, origin.x)?;
        let y = eval(1, origin.y)?;
        // Round shared edges, not each cell's width independently: thirds then
        // partition odd-sized work areas without gaps or overlapping pixels.
        let right = x + eval(2, 0)?;
        let bottom = y + eval(3, 0)?;
        if ![right, bottom]
            .iter()
            .all(|v| (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(v))
        {
            return Err("coordinate overflow / 坐标越界".into());
        }
        let rect = Rect {
            x: x.round() as i32,
            y: y.round() as i32,
            width: (right.round() as i32)
                .checked_sub(x.round() as i32)
                .ok_or("coordinate overflow")?,
            height: (bottom.round() as i32)
                .checked_sub(y.round() as i32)
                .ok_or("coordinate overflow")?,
        };
        if rect.width <= 0
            || rect.height <= 0
            || rect.x.checked_add(rect.width).is_none()
            || rect.y.checked_add(rect.height).is_none()
        {
            return Err("Invalid rectangle / 矩形越界".into());
        }
        Ok(rect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_terms_round_to_two_places_for_storage_and_resolution() {
        assert_eq!(
            normalize("1.234｜-2.345、33.33333sw+0.126|66.66666sh|2").unwrap(),
            "1.23|-2.35|33.33sw+0.13|66.67sh|2"
        );
        assert_eq!(
            normalize("0|0|100.00sw|50.5sh").unwrap(),
            "0|0|100sw|50.5sh"
        );
        let area = Rect {
            x: 0,
            y: 0,
            width: 10000,
            height: 10000,
        };
        let layout: Layout = "0|0|33.33333333sw|66.66666667sh".parse().unwrap();
        let resolved = layout.resolve(area, area, area).unwrap();
        assert_eq!((resolved.width, resolved.height), (3333, 6667));
        assert!(normalize("name 1/4").is_err());
        assert_eq!(normalize("0|0|100|100").unwrap(), "0|0|100|100");
    }
}
