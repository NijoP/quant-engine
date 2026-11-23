//! Inspectable explanation records.
//!
//! Every decision-producing function in this crate returns, alongside its
//! result, an [`Explanation`]: the model id, the formula applied, the exact
//! numeric inputs, the intermediate values and the outputs. Maps are
//! `BTreeMap`s so the JSON serialisation is byte-for-byte deterministic.
//! Non-finite values serialise as JSON `null`.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Explanation {
    pub model_id: String,
    pub formula: String,
    pub inputs: BTreeMap<String, f64>,
    pub intermediates: BTreeMap<String, f64>,
    pub outputs: BTreeMap<String, f64>,
    pub notes: Vec<String>,
    pub children: Vec<Explanation>,
}

impl Explanation {
    pub fn new(model_id: &str, formula: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
            formula: formula.to_string(),
            ..Default::default()
        }
    }
    pub fn input(mut self, k: &str, v: f64) -> Self {
        self.inputs.insert(k.to_string(), v);
        self
    }
    pub fn mid(mut self, k: &str, v: f64) -> Self {
        self.intermediates.insert(k.to_string(), v);
        self
    }
    pub fn out(mut self, k: &str, v: f64) -> Self {
        self.outputs.insert(k.to_string(), v);
        self
    }
    pub fn note(mut self, s: impl Into<String>) -> Self {
        self.notes.push(s.into());
        self
    }
    pub fn child(mut self, c: Explanation) -> Self {
        self.children.push(c);
        self
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_is_deterministic_and_nan_safe() {
        let e = Explanation::new("m", "y = x")
            .input("b", 2.0)
            .input("a", 1.0)
            .out("y", f64::NAN);
        let j = e.to_json();
        assert_eq!(j, e.clone().to_json());
        assert!(j.find("\"a\"").unwrap() < j.find("\"b\"").unwrap());
        assert!(j.contains("\"y\":null"));
    }
}
