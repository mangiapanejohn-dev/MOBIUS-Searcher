use crate::address::{Address, well_known};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub symbol: String,
    pub mint: Address,
    pub decimals: u8,
    /// Treated as $1.00 per whole token for USD valuation of quotes.
    #[serde(default)]
    pub usd_stable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TokenRegistry {
    by_symbol: BTreeMap<String, Token>,
    by_mint: HashMap<Address, String>,
}

impl TokenRegistry {
    pub fn insert(&mut self, t: Token) {
        self.by_mint.insert(t.mint, t.symbol.clone());
        self.by_symbol.insert(t.symbol.clone(), t);
    }

    pub fn get(&self, symbol: &str) -> Option<&Token> {
        self.by_symbol.get(symbol)
    }

    pub fn by_mint(&self, mint: &Address) -> Option<&Token> {
        self.by_mint.get(mint).and_then(|s| self.by_symbol.get(s))
    }

    /// Symbol for a mint, falling back to a shortened address.
    pub fn symbol(&self, mint: &Address) -> String {
        self.by_mint.get(mint).cloned().unwrap_or_else(|| mint.short())
    }

    pub fn decimals(&self, mint: &Address) -> Option<u8> {
        self.by_mint(mint).map(|t| t.decimals)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Token> {
        self.by_symbol.values()
    }

    pub fn sol(&self) -> &Token {
        self.get("SOL").expect("SOL is always registered")
    }

    /// SOL, USDC, USDT, JUP. Config may add more.
    pub fn defaults() -> Self {
        let mut r = TokenRegistry::default();
        let t = |symbol: &str, mint: &str, decimals: u8, usd_stable: bool| Token {
            symbol: symbol.into(),
            mint: well_known::addr(mint),
            decimals,
            usd_stable,
        };
        r.insert(t("SOL", well_known::WSOL_MINT, 9, false));
        r.insert(t("USDC", well_known::USDC_MINT, 6, true));
        r.insert(t("USDT", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", 6, true));
        r.insert(t("JUP", "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN", 6, false));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup() {
        let r = TokenRegistry::defaults();
        let usdc = r.get("USDC").unwrap();
        assert_eq!(r.by_mint(&usdc.mint).unwrap().symbol, "USDC");
        assert_eq!(r.sol().decimals, 9);
        assert_eq!(r.symbol(&Address([7; 32])).chars().count(), 9);
    }
}
