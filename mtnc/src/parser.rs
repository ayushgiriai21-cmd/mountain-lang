//! Recursive-descent parser with Pratt (operator-precedence) parsing for
//! expressions, per Document 17 §3's chosen implementation strategy and
//! Document 23's EBNF (ground truth for this phase per explicit
//! instruction). Binding-power table verified against Document 4 §10/§11
//! via an executed Python prototype (26/26 cases) before this port —
//! see the Phase 2 section of PROGRESS.md.

use crate::ast::*;
use crate::token::{Delim, Keyword, Op, Span, Token, TokenKind};

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

pub type ParseResult<T> = Result<T, ParseError>;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Suppressed while parsing `if`/`match`/`while`/`for` conditions or
    /// scrutinees, so `if x { ... }` isn't misparsed as a struct literal
    /// `x { }` followed by a block. See `parse_expr_no_struct_lit`.
    struct_lit_allowed: bool,
    pub errors: Vec<ParseError>,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        // Doc comments carry no grammatical meaning to the parser yet
        // (attaching them to AST nodes as metadata is future scope);
        // Error tokens were already reported by the lexer as diagnostics,
        // so the parser treats them as ordinary unexpected tokens rather
        // than double-reporting.
        let tokens: Vec<Token> = tokens
            .into_iter()
            .filter(|t| !matches!(t.kind, TokenKind::DocComment(_)))
            .collect();
        Parser { tokens, pos: 0, struct_lit_allowed: true, errors: Vec::new() }
    }

    // ---------- token stream helpers ----------

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_at(&self, offset: usize) -> &TokenKind {
        &self.tokens[(self.pos + offset).min(self.tokens.len() - 1)].kind
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn at_end(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Eof)
    }

    fn advance(&mut self) -> Token {
        let t = self.peek().clone();
        if !self.at_end() {
            self.pos += 1;
        }
        t
    }

    fn here(&self) -> Span {
        self.peek().span
    }

    fn error<T>(&self, msg: impl Into<String>) -> ParseResult<T> {
        Err(ParseError { message: msg.into(), span: self.here() })
    }

    fn check_kw(&self, kw: Keyword) -> bool {
        matches!(self.peek_kind(), TokenKind::Keyword(k) if *k == kw)
    }
    fn check_op(&self, op: Op) -> bool {
        matches!(self.peek_kind(), TokenKind::Op(o) if *o == op)
    }
    fn check_delim(&self, d: Delim) -> bool {
        matches!(self.peek_kind(), TokenKind::Delim(dd) if *dd == d)
    }
    fn check_ident(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Ident(_))
    }

    fn eat_kw(&mut self, kw: Keyword) -> bool {
        if self.check_kw(kw) { self.advance(); true } else { false }
    }
    fn eat_op(&mut self, op: Op) -> bool {
        if self.check_op(op) { self.advance(); true } else { false }
    }
    fn eat_delim(&mut self, d: Delim) -> bool {
        if self.check_delim(d) { self.advance(); true } else { false }
    }

    fn expect_kw(&mut self, kw: Keyword) -> ParseResult<()> {
        if self.eat_kw(kw) { Ok(()) } else {
            self.error(format!("expected keyword `{:?}`, found {:?}", kw, self.peek_kind()))
        }
    }
    fn expect_op(&mut self, op: Op) -> ParseResult<()> {
        if self.eat_op(op) { Ok(()) } else {
            self.error(format!("expected `{}`, found {:?}", op, self.peek_kind()))
        }
    }
    fn expect_delim(&mut self, d: Delim) -> ParseResult<()> {
        if self.eat_delim(d) { Ok(()) } else {
            self.error(format!("expected `{}`, found {:?}", d, self.peek_kind()))
        }
    }
    fn expect_ident(&mut self) -> ParseResult<String> {
        match self.peek_kind().clone() {
            TokenKind::Ident(s) => { self.advance(); Ok(s) }
            other => self.error(format!("expected identifier, found {:?}", other)),
        }
    }
    /// Accepts a plain identifier OR a keyword being used as an ordinary
    /// word — needed because several Document 3 keywords double as real
    /// names in practice: `query`/`table` as module-path segments
    /// (Document 24 §1's `use std::db::query;`), and `on`/`bind` as
    /// named-argument labels (Document 24 §2's `Button(..., on: click =>
    /// ...)`, `TextInput(bind: newTitle, ...)`). Used at every name-like
    /// position where this collision is real; left as plain
    /// `expect_ident` for declaration sites (fn/struct/variable names)
    /// where no example shows a keyword being used this way.
    fn expect_word(&mut self) -> ParseResult<String> {
        if let TokenKind::Ident(s) = self.peek_kind().clone() {
            self.advance();
            return Ok(s);
        }
        if let TokenKind::Keyword(k) = self.peek_kind().clone() {
            self.advance();
            return Ok(k.as_source_text());
        }
        self.error(format!("expected identifier, found {:?}", self.peek_kind()))
    }
    /// Some AST positions accept either a plain identifier OR a keyword
    /// being used as a name-like token (e.g. `Self`, or a keyword that's
    /// contextually a query-clause name). Used sparingly and only where
    /// Document 23 grammar clearly intends a bare word, not full
    /// arbitrary-keyword-as-identifier support.
    fn expect_ident_or(&mut self, extra_ok: &[Keyword]) -> ParseResult<String> {
        if let TokenKind::Keyword(k) = self.peek_kind().clone() {
            if extra_ok.contains(&k) {
                self.advance();
                return Ok(format!("{:?}", k));
            }
        }
        self.expect_ident()
    }

    /// Consumes zero or more `::segment` path continuations after an
    /// already-consumed first segment, stopping BEFORE a `::<...>`
    /// turbofish (left for callers that care about it to handle
    /// separately). Shared by type parsing and expression-path parsing
    /// so a multi-segment path like `orderbook::OrderBook` (Document 24
    /// §4) or `layers::Dense` (Document 24 §3) parses correctly in
    /// both positions from one fix, rather than two separate ones —
    /// this was traced to be the same root cause for both failures, not
    /// a coincidence. Also the reason `channel::<i32>()` (Document 24
    /// §5) now parses: previously the path-building loop unconditionally
    /// tried to consume another word after every `::`, including when
    /// the `::` was actually introducing generic args, so it choked on
    /// the `<` instead of leaving it for the turbofish-handling code
    /// that already existed right after the loop.
    fn parse_further_path_segments(&mut self) -> ParseResult<Vec<String>> {
        let mut extra = Vec::new();
        loop {
            if !self.check_op(Op::ColonColon) { break; }
            let save = self.pos;
            self.advance();
            if self.check_op(Op::Lt) {
                self.pos = save;
                break;
            }
            extra.push(self.expect_word()?);
        }
        Ok(extra)
    }

    // ---------- top level ----------

    pub fn parse_program(&mut self) -> Program {
        let mut items = Vec::new();
        while !self.at_end() {
            match self.parse_item() {
                Ok(item) => items.push(item),
                Err(e) => {
                    self.errors.push(e);
                    self.synchronize_item();
                }
            }
        }
        Program { items }
    }

    /// Error recovery (Document 17 §2 / Document 25 Phase 1 precedent
    /// extended to the parser): on a parse error at item level, skip
    /// tokens until a plausible new-item boundary so one bad top-level
    /// declaration doesn't abort parsing of the whole file.
    fn synchronize_item(&mut self) {
        while !self.at_end() {
            if self.check_delim(Delim::Semi) {
                self.advance();
                return;
            }
            if item_start_keyword(self.peek_kind()) {
                return;
            }
            self.advance();
        }
    }

    fn parse_attrs(&mut self) -> ParseResult<Vec<Attribute>> {
        let mut attrs = Vec::new();
        while self.check_op(Op::At) {
            self.advance();
            let name = self.expect_word()?;
            let mut args = Vec::new();
            if self.eat_delim(Delim::LParen) {
                if !self.check_delim(Delim::RParen) {
                    args.push(self.parse_expr(0)?);
                    while self.eat_delim(Delim::Comma) {
                        if self.check_delim(Delim::RParen) { break; }
                        args.push(self.parse_expr(0)?);
                    }
                }
                self.expect_delim(Delim::RParen)?;
            }
            attrs.push(Attribute { name, args });
        }
        Ok(attrs)
    }

    fn parse_visibility(&mut self) -> ParseResult<Visibility> {
        if self.eat_kw(Keyword::Pub) {
            if self.eat_delim(Delim::LParen) {
                let word = self.expect_ident_or(&[])?;
                self.expect_delim(Delim::RParen)?;
                if word == "package" {
                    return Ok(Visibility::PubPackage);
                }
                return self.error("expected `package` inside pub(...)");
            }
            return Ok(Visibility::Pub);
        }
        Ok(Visibility::Private)
    }

    fn parse_item(&mut self) -> ParseResult<Item> {
        let span = self.here();
        let attrs = self.parse_attrs()?;
        let visibility = self.parse_visibility()?;

        // #target(...) { items }  (item-position form)
        if self.check_op(Op::Hash) {
            let (kind, block_items) = self.parse_target_block_items()?;
            return Ok(Item {
                attrs, visibility,
                kind: ItemKind::TargetBlock(kind, block_items),
                span,
            });
        }

        let kind = if self.check_kw(Keyword::Async) || self.check_kw(Keyword::Fn) {
            ItemKind::Fn(self.parse_fn_decl()?)
        } else if self.check_kw(Keyword::Struct) {
            ItemKind::Struct(self.parse_struct_decl()?)
        } else if self.check_kw(Keyword::Enum) {
            ItemKind::Enum(self.parse_enum_decl()?)
        } else if self.check_kw(Keyword::Trait) {
            ItemKind::Trait(self.parse_trait_decl()?)
        } else if self.check_kw(Keyword::Impl) {
            ItemKind::Impl(self.parse_impl_decl()?)
        } else if self.check_kw(Keyword::Mod) {
            ItemKind::Mod(self.parse_mod_decl()?)
        } else if self.check_kw(Keyword::Use) {
            ItemKind::Use(self.parse_use_decl()?)
        } else if self.check_kw(Keyword::Import) {
            ItemKind::Import(self.parse_import_decl()?)
        } else if self.check_kw(Keyword::Const) {
            ItemKind::Const(self.parse_const_decl()?)
        } else if self.check_kw(Keyword::Static) {
            ItemKind::Static(self.parse_static_decl()?)
        } else if self.check_kw(Keyword::Type) {
            ItemKind::TypeAlias(self.parse_type_alias()?)
        } else if self.check_kw(Keyword::Table) {
            ItemKind::Table(self.parse_table_decl()?)
        } else if self.check_kw(Keyword::Index) {
            ItemKind::Index(self.parse_index_decl()?)
        } else if self.check_kw(Keyword::Schema) {
            ItemKind::Schema(self.parse_schema_decl()?)
        } else if self.check_kw(Keyword::Ui) {
            self.advance();
            ItemKind::Ui(self.parse_ui_body()?)
        } else if self.check_kw(Keyword::Component) {
            self.advance();
            ItemKind::Component(self.parse_ui_body()?)
        } else if self.check_kw(Keyword::Server) {
            ItemKind::Server(self.parse_server_decl()?)
        } else if self.check_kw(Keyword::Actor) {
            ItemKind::Actor(self.parse_actor_decl()?)
        } else {
            return self.error(format!("expected item, found {:?}", self.peek_kind()));
        };

        Ok(Item { attrs, visibility, kind, span })
    }

    fn parse_target_kind(&mut self) -> ParseResult<TargetKind> {
        self.expect_op(Op::Hash)?;
        let name = self.expect_word()?;
        if name != "target" {
            return self.error("expected `target` after `#`");
        }
        self.expect_delim(Delim::LParen)?;
        let word = self.expect_word()?;
        let kind = match word.as_str() {
            "native" => TargetKind::Native,
            "wasm" => TargetKind::Wasm,
            "all" => TargetKind::All,
            _ => return self.error("expected native|wasm|all inside #target(...)"),
        };
        self.expect_delim(Delim::RParen)?;
        Ok(kind)
    }

    fn parse_target_block_items(&mut self) -> ParseResult<(TargetKind, Vec<Item>)> {
        let kind = self.parse_target_kind()?;
        self.expect_delim(Delim::LBrace)?;
        let mut items = Vec::new();
        while !self.check_delim(Delim::RBrace) && !self.at_end() {
            items.push(self.parse_item()?);
        }
        self.expect_delim(Delim::RBrace)?;
        Ok((kind, items))
    }

    // ---------- generics / where ----------

    fn parse_generic_params(&mut self) -> ParseResult<GenericParams> {
        if !self.check_op(Op::Lt) {
            return Ok(GenericParams::default());
        }
        self.advance();
        let mut params = Vec::new();
        loop {
            if self.check_op(Op::Gt) { break; }
            if self.eat_kw(Keyword::Const) {
                let name = self.expect_word()?;
                self.expect_delim(Delim::Colon)?;
                let ty = self.parse_type()?;
                params.push(GenericParam::Const { name, ty });
            } else {
                let name = self.expect_word()?;
                let mut bounds = Vec::new();
                if self.eat_delim(Delim::Colon) {
                    bounds.push(self.parse_trait_bound()?);
                    while self.eat_op(Op::Plus) {
                        bounds.push(self.parse_trait_bound()?);
                    }
                }
                params.push(GenericParam::Type { name, bounds });
            }
            if !self.eat_delim(Delim::Comma) { break; }
        }
        self.expect_op(Op::Gt)?;
        Ok(GenericParams(params))
    }

    fn parse_trait_bound(&mut self) -> ParseResult<TraitBound> {
        let name = self.expect_word()?;
        let args = self.parse_generic_args_opt()?;
        Ok(TraitBound { name, args })
    }

    fn parse_generic_args_opt(&mut self) -> ParseResult<Vec<Type>> {
        if !self.check_op(Op::Lt) {
            return Ok(Vec::new());
        }
        self.advance();
        let mut args = Vec::new();
        if !self.check_op(Op::Gt) {
            args.push(self.parse_generic_arg()?);
            while self.eat_delim(Delim::Comma) {
                args.push(self.parse_generic_arg()?);
            }
        }
        self.expect_op(Op::Gt)?;
        Ok(args)
    }

    /// A single generic argument: a type in the common case, or a
    /// const-generic value (Document 8 §8's `Matrix<f64, 2, 3>`) when
    /// the argument position starts with a literal rather than a type.
    fn parse_generic_arg(&mut self) -> ParseResult<Type> {
        if matches!(self.peek_kind(), TokenKind::Int(_) | TokenKind::IntHex(_)
            | TokenKind::IntOct(_) | TokenKind::IntBin(_)) {
            let lit = self.parse_literal()?;
            return Ok(Type::ConstArg(Box::new(Expr::Literal(lit))));
        }
        self.parse_type()
    }

    fn parse_where_clause(&mut self) -> ParseResult<WhereClause> {
        if !self.eat_kw(Keyword::Where) {
            return Ok(WhereClause::default());
        }
        let mut bounds = Vec::new();
        loop {
            let name = self.expect_word()?;
            self.expect_delim(Delim::Colon)?;
            let mut tb = vec![self.parse_trait_bound()?];
            while self.eat_op(Op::Plus) {
                tb.push(self.parse_trait_bound()?);
            }
            bounds.push(WhereBound { name, bounds: tb });
            if !self.eat_delim(Delim::Comma) { break; }
            if self.check_delim(Delim::LBrace) { break; }
        }
        Ok(WhereClause(bounds))
    }

    // ---------- fn ----------

    fn parse_fn_decl(&mut self) -> ParseResult<FnDecl> {
        let is_async = self.eat_kw(Keyword::Async);
        self.expect_kw(Keyword::Fn)?;
        let name = self.expect_word()?;
        let generics = self.parse_generic_params()?;
        self.expect_delim(Delim::LParen)?;
        let mut params = Vec::new();
        if !self.check_delim(Delim::RParen) {
            params.push(self.parse_param()?);
            while self.eat_delim(Delim::Comma) {
                if self.check_delim(Delim::RParen) { break; }
                params.push(self.parse_param()?);
            }
        }
        self.expect_delim(Delim::RParen)?;
        let return_type = if self.eat_op(Op::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let where_clause = self.parse_where_clause()?;
        let body = if self.check_delim(Delim::Semi) {
            self.advance();
            None
        } else {
            Some(self.parse_block()?)
        };
        Ok(FnDecl { is_async, name, generics, params, return_type, where_clause, body })
    }

    fn parse_param(&mut self) -> ParseResult<Param> {
        if self.eat_op(Op::DotDot) {
            // variadic: "..." IDENT ":" type  -- lexer produces DotDot
            // then a separate Dot for the third '.'; accept either
            // '...'-as-three-dots spelling by also consuming a trailing Dot.
            self.eat_op(Op::Dot);
            let name = self.expect_word()?;
            self.expect_delim(Delim::Colon)?;
            let ty = self.parse_type()?;
            return Ok(Param { ownership: OwnershipMod::None, name, is_variadic: true, ty, default: None });
        }
        let ownership = if self.eat_kw(Keyword::Borrow) {
            if self.eat_kw(Keyword::Mut) { OwnershipMod::BorrowMut } else { OwnershipMod::Borrow }
        } else if self.eat_kw(Keyword::Move) {
            OwnershipMod::Move
        } else {
            OwnershipMod::None
        };
        // `self`-shaped receiver params: `self`, `borrow self`, `borrow mut self`
        let name = if self.check_kw(Keyword::SelfValue) {
            self.advance();
            "self".to_string()
        } else {
            self.expect_word()?
        };
        if name == "self" {
            return Ok(Param { ownership, name, is_variadic: false, ty: Type::Unit, default: None });
        }
        self.expect_delim(Delim::Colon)?;
        let ty = self.parse_type()?;
        let default = if self.eat_op(Op::Eq) { Some(self.parse_expr(0)?) } else { None };
        Ok(Param { ownership, name, is_variadic: false, ty, default })
    }

    // ---------- struct/enum/trait/impl ----------

    fn parse_struct_decl(&mut self) -> ParseResult<StructDecl> {
        self.expect_kw(Keyword::Struct)?;
        let name = self.expect_word()?;
        let generics = self.parse_generic_params()?;
        let body = if self.eat_delim(Delim::Semi) {
            StructBody::Unit
        } else if self.check_delim(Delim::LParen) {
            self.advance();
            let mut tys = Vec::new();
            if !self.check_delim(Delim::RParen) {
                tys.push(self.parse_type()?);
                while self.eat_delim(Delim::Comma) {
                    if self.check_delim(Delim::RParen) { break; }
                    tys.push(self.parse_type()?);
                }
            }
            self.expect_delim(Delim::RParen)?;
            self.expect_delim(Delim::Semi)?;
            StructBody::Tuple(tys)
        } else {
            self.expect_delim(Delim::LBrace)?;
            let mut fields = Vec::new();
            while !self.check_delim(Delim::RBrace) {
                let visibility = self.parse_visibility()?;
                let name = self.expect_word()?;
                self.expect_delim(Delim::Colon)?;
                let ty = self.parse_type()?;
                fields.push(FieldDecl { visibility, name, ty });
                if !self.eat_delim(Delim::Comma) { break; }
            }
            self.expect_delim(Delim::RBrace)?;
            StructBody::Named(fields)
        };
        Ok(StructDecl { name, generics, body })
    }

    fn parse_enum_decl(&mut self) -> ParseResult<EnumDecl> {
        self.expect_kw(Keyword::Enum)?;
        let name = self.expect_word()?;
        let generics = self.parse_generic_params()?;
        self.expect_delim(Delim::LBrace)?;
        let mut variants = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            let vname = self.expect_word()?;
            let mut data = Vec::new();
            if self.eat_delim(Delim::LParen) {
                if !self.check_delim(Delim::RParen) {
                    data.push(self.parse_type()?);
                    while self.eat_delim(Delim::Comma) {
                        if self.check_delim(Delim::RParen) { break; }
                        data.push(self.parse_type()?);
                    }
                }
                self.expect_delim(Delim::RParen)?;
            }
            variants.push(EnumVariant { name: vname, data });
            if !self.eat_delim(Delim::Comma) { break; }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(EnumDecl { name, generics, variants })
    }

    fn parse_trait_decl(&mut self) -> ParseResult<TraitDecl> {
        self.expect_kw(Keyword::Trait)?;
        let name = self.expect_word()?;
        let generics = self.parse_generic_params()?;
        self.expect_delim(Delim::LBrace)?;
        let mut items = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            if self.eat_kw(Keyword::Type) {
                let n = self.expect_word()?;
                self.expect_delim(Delim::Semi)?;
                items.push(TraitItem::AssocType(n));
            } else {
                items.push(TraitItem::Fn(self.parse_fn_decl()?));
            }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(TraitDecl { name, generics, items })
    }

    fn parse_type_ref(&mut self) -> ParseResult<TypeRef> {
        let name = self.expect_word()?;
        let args = self.parse_generic_args_opt()?;
        Ok(TypeRef { name, args })
    }

    fn parse_impl_decl(&mut self) -> ParseResult<ImplDecl> {
        self.expect_kw(Keyword::Impl)?;
        let generics = self.parse_generic_params()?;
        let first = self.parse_type_ref()?;
        let (trait_ref, target) = if self.eat_kw(Keyword::For) {
            let target = self.parse_type_ref()?;
            (Some(first), target)
        } else {
            (None, first)
        };
        let where_clause = self.parse_where_clause()?;
        self.expect_delim(Delim::LBrace)?;
        let mut items = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            if self.eat_kw(Keyword::Type) {
                let n = self.expect_word()?;
                self.expect_op(Op::Eq)?;
                let ty = self.parse_type()?;
                self.expect_delim(Delim::Semi)?;
                items.push(ImplItem::AssocType(n, ty));
            } else {
                items.push(ImplItem::Fn(self.parse_fn_decl()?));
            }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(ImplDecl { generics, trait_ref, target, where_clause, items })
    }

    fn parse_mod_decl(&mut self) -> ParseResult<ModDecl> {
        self.expect_kw(Keyword::Mod)?;
        let name = self.expect_word()?;
        self.expect_delim(Delim::LBrace)?;
        let mut items = Vec::new();
        while !self.check_delim(Delim::RBrace) && !self.at_end() {
            items.push(self.parse_item()?);
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(ModDecl { name, items })
    }

    fn parse_use_decl(&mut self) -> ParseResult<UsePath> {
        self.expect_kw(Keyword::Use)?;
        let mut path = vec![self.expect_word()?];
        let mut items = Vec::new();
        while self.eat_op(Op::ColonColon) {
            if self.eat_delim(Delim::LBrace) {
                items.push(self.expect_word()?);
                while self.eat_delim(Delim::Comma) {
                    if self.check_delim(Delim::RBrace) { break; }
                    items.push(self.expect_word()?);
                }
                self.expect_delim(Delim::RBrace)?;
                break;
            }
            path.push(self.expect_word()?);
        }
        self.expect_delim(Delim::Semi)?;
        Ok(UsePath { path, items })
    }

    fn parse_import_decl(&mut self) -> ParseResult<Vec<String>> {
        self.expect_kw(Keyword::Import)?;
        let mut path = vec![self.expect_word()?];
        while self.eat_op(Op::ColonColon) {
            path.push(self.expect_word()?);
        }
        self.expect_delim(Delim::Semi)?;
        Ok(path)
    }

    fn parse_const_decl(&mut self) -> ParseResult<ConstDecl> {
        self.expect_kw(Keyword::Const)?;
        let name = self.expect_word()?;
        self.expect_delim(Delim::Colon)?;
        let ty = self.parse_type()?;
        self.expect_op(Op::Eq)?;
        let value = self.parse_expr(0)?;
        self.expect_delim(Delim::Semi)?;
        Ok(ConstDecl { name, ty, value })
    }

    fn parse_static_decl(&mut self) -> ParseResult<StaticDecl> {
        self.expect_kw(Keyword::Static)?;
        let name = self.expect_word()?;
        self.expect_delim(Delim::Colon)?;
        let ty = self.parse_type()?;
        self.expect_op(Op::Eq)?;
        let value = self.parse_expr(0)?;
        self.expect_delim(Delim::Semi)?;
        Ok(StaticDecl { name, ty, value })
    }

    fn parse_type_alias(&mut self) -> ParseResult<TypeAliasDecl> {
        self.expect_kw(Keyword::Type)?;
        let name = self.expect_word()?;
        let generics = self.parse_generic_params()?;
        self.expect_op(Op::Eq)?;
        let ty = self.parse_type()?;
        self.expect_delim(Delim::Semi)?;
        Ok(TypeAliasDecl { name, generics, ty })
    }

    // ---------- database ----------

    fn parse_table_decl(&mut self) -> ParseResult<TableDecl> {
        self.expect_kw(Keyword::Table)?;
        let name = self.expect_word()?;
        self.expect_delim(Delim::LBrace)?;
        let mut fields = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            let fname = self.expect_word()?;
            self.expect_delim(Delim::Colon)?;
            let ty = self.parse_type()?;
            let mut constraints = Vec::new();
            loop {
                if self.check_ident() {
                    // constraint keywords are lexed as plain identifiers
                    // (Document 20 §2's `primary_key`, `auto_increment`,
                    // `unique`, `not_null` are not in Document 3's
                    // keyword list at all — treated the same way
                    // primitive type names are: recognized by the
                    // parser via identifier text, not lexer keywords).
                    let word = if let TokenKind::Ident(w) = self.peek_kind().clone() { w } else { unreachable!() };
                    match word.as_str() {
                        "primary_key" => { self.advance(); constraints.push(FieldConstraint::PrimaryKey); }
                        "auto_increment" => { self.advance(); constraints.push(FieldConstraint::AutoIncrement); }
                        "unique" => { self.advance(); constraints.push(FieldConstraint::Unique); }
                        "not_null" => { self.advance(); constraints.push(FieldConstraint::NotNull); }
                        "default" => {
                            self.advance();
                            self.expect_delim(Delim::LParen)?;
                            let e = self.parse_expr(0)?;
                            self.expect_delim(Delim::RParen)?;
                            constraints.push(FieldConstraint::Default(e));
                        }
                        _ => break,
                    }
                } else {
                    break;
                }
            }
            fields.push(TableField { name: fname, ty, constraints });
            if !self.eat_delim(Delim::Comma) { break; }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(TableDecl { name, fields })
    }

    fn parse_index_decl(&mut self) -> ParseResult<IndexDecl> {
        self.expect_kw(Keyword::Index)?;
        let name = self.expect_word()?;
        self.expect_kw(Keyword::On)?;
        let table = self.expect_word()?;
        self.expect_delim(Delim::LParen)?;
        let column = self.expect_word()?;
        self.expect_delim(Delim::RParen)?;
        let using_hash = if self.check_ident() {
            // `using hash` / `using btree` — "using" also isn't a
            // Document 3 keyword; same treatment as table constraints.
            if let TokenKind::Ident(w) = self.peek_kind().clone() {
                if w == "using" {
                    self.advance();
                    let mode = self.expect_word()?;
                    mode == "hash"
                } else { false }
            } else { false }
        } else { false };
        self.expect_delim(Delim::Semi)?;
        Ok(IndexDecl { name, table, column, using_hash })
    }

    fn parse_schema_decl(&mut self) -> ParseResult<SchemaDecl> {
        self.expect_kw(Keyword::Schema)?;
        let name = self.expect_word()?;
        self.expect_delim(Delim::LBrace)?;
        let mut versions = Vec::new();
        while self.check_ident() {
            let word = if let TokenKind::Ident(w) = self.peek_kind().clone() { w } else { unreachable!() };
            if word != "version" { break; }
            self.advance();
            let num_tok = self.advance();
            let num = match num_tok.kind {
                TokenKind::Int(s) => s.parse::<u64>().unwrap_or(0),
                _ => return self.error("expected integer version number"),
            };
            let block = self.parse_block()?;
            let items: Vec<Item> = block.stmts.into_iter().filter_map(|s| match s {
                Stmt::Item(i) => Some(*i),
                _ => None,
            }).collect();
            versions.push((num, items));
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(SchemaDecl { name, versions })
    }

    // ---------- networking ----------

    fn parse_server_decl(&mut self) -> ParseResult<ServerDecl> {
        self.expect_kw(Keyword::Server)?;
        let name = self.expect_word()?;
        self.expect_delim(Delim::LBrace)?;
        let mut items = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            if self.eat_kw(Keyword::Listen) {
                let args = self.parse_arg_list()?;
                self.expect_delim(Delim::Semi)?;
                items.push(ServerItem::Listen(args));
            } else if self.eat_kw(Keyword::On) {
                let name = self.expect_word()?;
                self.expect_delim(Delim::LParen)?;
                let mut params = Vec::new();
                if !self.check_delim(Delim::RParen) {
                    params.push(self.parse_param()?);
                    while self.eat_delim(Delim::Comma) {
                        if self.check_delim(Delim::RParen) { break; }
                        params.push(self.parse_param()?);
                    }
                }
                self.expect_delim(Delim::RParen)?;
                let body = self.parse_block()?;
                items.push(ServerItem::On(OnHandler { name, params, body }));
            } else {
                return self.error("expected `listen` or `on` inside server block");
            }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(ServerDecl { name, items })
    }

    // ---------- concurrency: actor ----------

    fn parse_actor_decl(&mut self) -> ParseResult<ActorDecl> {
        self.expect_kw(Keyword::Actor)?;
        let name = self.expect_word()?;
        self.expect_delim(Delim::LBrace)?;
        let mut items = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            if self.eat_kw(Keyword::State) {
                let sname = self.expect_word()?;
                self.expect_delim(Delim::Colon)?;
                let ty = self.parse_type()?;
                self.expect_op(Op::Eq)?;
                let value = self.parse_expr(0)?;
                self.expect_delim(Delim::Semi)?;
                items.push(ActorItem::State(StateDecl { name: sname, ty, value }));
            } else if self.eat_kw(Keyword::On) {
                let name = self.expect_word()?;
                self.expect_delim(Delim::LParen)?;
                let mut params = Vec::new();
                if !self.check_delim(Delim::RParen) {
                    params.push(self.parse_param()?);
                    while self.eat_delim(Delim::Comma) {
                        if self.check_delim(Delim::RParen) { break; }
                        params.push(self.parse_param()?);
                    }
                }
                self.expect_delim(Delim::RParen)?;
                let body = self.parse_block()?;
                items.push(ActorItem::On(OnHandler { name, params, body }));
            } else {
                return self.error("expected `state` or `on` inside actor block");
            }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(ActorDecl { name, items })
    }

    // ---------- UI ----------

    fn parse_ui_body(&mut self) -> ParseResult<UiDecl> {
        let name = self.expect_word()?;
        self.expect_delim(Delim::LBrace)?;
        let mut items = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            if self.eat_kw(Keyword::State) {
                let sname = self.expect_word()?;
                self.expect_delim(Delim::Colon)?;
                let ty = self.parse_type()?;
                self.expect_op(Op::Eq)?;
                let value = self.parse_expr(0)?;
                self.expect_delim(Delim::Semi)?;
                items.push(UiItem::State(StateDecl { name: sname, ty, value }));
            } else if self.eat_kw(Keyword::Prop) {
                let pname = self.expect_word()?;
                self.expect_delim(Delim::Colon)?;
                let ty = self.parse_type()?;
                self.expect_delim(Delim::Semi)?;
                items.push(UiItem::Prop(PropDecl { name: pname, ty }));
            } else if self.eat_kw(Keyword::Render) {
                items.push(UiItem::Render(self.parse_block()?));
            } else if self.eat_kw(Keyword::Mount) {
                items.push(UiItem::Mount(self.parse_block()?));
            } else if self.eat_kw(Keyword::Unmount) {
                items.push(UiItem::Unmount(self.parse_block()?));
            } else if self.check_kw(Keyword::Async) || self.check_kw(Keyword::Fn) {
                // See ast.rs's UiItem::Fn doc comment: flagged deviation
                // from Document 23 §10, required for Document 24 §2 to
                // round-trip.
                items.push(UiItem::Fn(self.parse_fn_decl()?));
            } else {
                return self.error(format!(
                    "expected state/prop/render/mount/unmount/fn inside ui/component block, found {:?}",
                    self.peek_kind()
                ));
            }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(UiDecl { name, items })
    }

    // ---------- types ----------

    fn parse_type(&mut self) -> ParseResult<Type> {
        if self.eat_delim(Delim::LParen) {
            if self.eat_delim(Delim::RParen) {
                return Ok(Type::Unit);
            }
            let mut tys = vec![self.parse_type()?];
            let mut is_tuple = false;
            while self.eat_delim(Delim::Comma) {
                is_tuple = true;
                if self.check_delim(Delim::RParen) { break; }
                tys.push(self.parse_type()?);
            }
            self.expect_delim(Delim::RParen)?;
            return Ok(if is_tuple { Type::Tuple(tys) } else { tys.into_iter().next().unwrap() });
        }
        if self.eat_op(Op::Not) {
            // Document 5 §2.5's never type is spelled `!`; the lexer
            // produces Op::Not for a bare '!'.
            return Ok(Type::Never);
        }
        if self.eat_delim(Delim::LBracket) {
            let elem = self.parse_type()?;
            let size = if self.eat_delim(Delim::Semi) {
                Some(Box::new(self.parse_expr(0)?))
            } else {
                None
            };
            self.expect_delim(Delim::RBracket)?;
            return Ok(Type::Array(Box::new(elem), size));
        }
        if self.eat_op(Op::Amp) {
            // Lifetimes ('a syntax) aren't separately tokenized by the
            // Phase 1 lexer (no dedicated lifetime token kind exists
            // yet — flagged gap, see PROGRESS.md), so `lifetime` is
            // always None here for now.
            let lifetime = None;
            let mutable = self.eat_kw(Keyword::Mut);
            let inner = self.parse_type()?;
            return Ok(Type::Ref { lifetime, mutable, inner: Box::new(inner) });
        }
        if self.eat_kw(Keyword::Borrow) {
            // `borrow Type` / `borrow mut Type` in TYPE position (as
            // opposed to `borrow`/`borrow mut` as a *parameter*
            // ownership modifier before the param name, which
            // `parse_param` already handles separately) — Document 7
            // §4.1's own canonical trait example uses this exact form:
            // `fn compareTo(borrow self, other: borrow Self) -> i32;`.
            // Previously `parse_type()` had no branch for
            // `Keyword::Borrow` at all, so it fell through to the
            // generic keyword-as-word fallback and silently mis-parsed
            // "borrow" itself as a bogus type name (`Type::Named("borrow")`),
            // with the resulting error only surfacing later at the
            // closing paren once the leftover `Self` token didn't match
            // anything expected. Fixed here, once, in `parse_type`
            // itself (not per-call-site), so this is closed uniformly
            // for every type position that flows through this function
            // — return types, struct fields, generic arguments,
            // where-clause bound types — not just function parameters.
            // Reuses `Type::Ref` (the same AST shape `&Type` already
            // produces just above) rather than inventing a new variant,
            // since `borrow Type` and `&Type` are the same underlying
            // concept spelled two ways (Document 6 §3's `ref x` note
            // makes the same equivalence for the expression-position
            // spelling).
            let mutable = self.eat_kw(Keyword::Mut);
            let inner = self.parse_type()?;
            return Ok(Type::Ref { lifetime: None, mutable, inner: Box::new(inner) });
        }
        if self.eat_kw(Keyword::Dyn) {
            let name = self.expect_word()?;
            let args = self.parse_generic_args_opt()?;
            return Ok(Type::Dyn(name, args));
        }
        if self.eat_kw(Keyword::Fn) {
            self.expect_delim(Delim::LParen)?;
            let mut params = Vec::new();
            if !self.check_delim(Delim::RParen) {
                params.push(self.parse_type()?);
                while self.eat_delim(Delim::Comma) {
                    if self.check_delim(Delim::RParen) { break; }
                    params.push(self.parse_type()?);
                }
            }
            self.expect_delim(Delim::RParen)?;
            self.expect_op(Op::Arrow)?;
            let ret = self.parse_type()?;
            return Ok(Type::Fn(params, Box::new(ret)));
        }
        if self.eat_kw(Keyword::Option) {
            self.expect_op(Op::Lt)?;
            let inner = self.parse_type()?;
            self.expect_op(Op::Gt)?;
            return Ok(Type::Option(Box::new(inner)));
        }
        if self.eat_kw(Keyword::Result) {
            self.expect_op(Op::Lt)?;
            let ok = self.parse_type()?;
            self.expect_delim(Delim::Comma)?;
            let err = self.parse_type()?;
            self.expect_op(Op::Gt)?;
            return Ok(Type::Result(Box::new(ok), Box::new(err)));
        }
        // plain name -- either a primitive (recognized by text, per the
        // Phase 1 design decision that primitives lex as identifiers)
        // or a user-defined named type, optionally generic. May be a
        // multi-segment path (`orderbook::OrderBook`, `layers::Dense`)
        // -- see `parse_further_path_segments`'s doc comment for why
        // this was previously missing entirely.
        let mut name = if self.check_kw(Keyword::SelfType) {
            self.advance();
            "Self".to_string()
        } else {
            self.expect_word()?
        };
        for seg in self.parse_further_path_segments()? {
            name = format!("{}::{}", name, seg);
        }
        if is_primitive_type_name(&name) {
            return Ok(Type::Primitive(name));
        }
        let args = self.parse_generic_args_opt()?;
        Ok(Type::Named(name, args))
    }

    // ---------- patterns ----------

    fn parse_pattern(&mut self) -> ParseResult<Pattern> {
        let first = self.parse_pattern_single()?;
        if self.check_op(Op::Pipe) {
            let mut pats = vec![first];
            while self.eat_op(Op::Pipe) {
                pats.push(self.parse_pattern_single()?);
            }
            return Ok(Pattern::Or(pats));
        }
        Ok(first)
    }

    fn parse_pattern_single(&mut self) -> ParseResult<Pattern> {
        if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "_") {
            self.advance();
            return Ok(Pattern::Wildcard);
        }
        if self.eat_kw(Keyword::Mut) {
            let name = self.expect_word()?;
            return Ok(Pattern::Mut(name));
        }
        if self.check_delim(Delim::LParen) {
            self.advance();
            let mut pats = Vec::new();
            if !self.check_delim(Delim::RParen) {
                pats.push(self.parse_pattern()?);
                while self.eat_delim(Delim::Comma) {
                    if self.check_delim(Delim::RParen) { break; }
                    pats.push(self.parse_pattern()?);
                }
            }
            self.expect_delim(Delim::RParen)?;
            return Ok(Pattern::Tuple(pats));
        }
        if self.check_delim(Delim::LBracket) {
            self.advance();
            let mut pats = Vec::new();
            let mut rest = None;
            while !self.check_delim(Delim::RBracket) {
                if self.eat_op(Op::DotDot) {
                    let bind = if self.check_ident() { Some(self.expect_word()?) } else { None };
                    rest = Some(bind);
                    break;
                }
                pats.push(self.parse_pattern()?);
                if !self.eat_delim(Delim::Comma) { break; }
            }
            self.expect_delim(Delim::RBracket)?;
            return Ok(Pattern::Array(pats, rest));
        }
        if matches!(self.peek_kind(), TokenKind::Int(_) | TokenKind::Float(_) | TokenKind::Str(_)
            | TokenKind::Char(_) | TokenKind::Bool(_) | TokenKind::IntHex(_)
            | TokenKind::IntOct(_) | TokenKind::IntBin(_)) {
            return Ok(Pattern::Literal(self.parse_literal()?));
        }
        // identifier, possibly a path (Enum::Variant), possibly followed
        // by a tuple-struct pattern's parenthesized sub-patterns.
        // Uses `expect_word` (not `expect_ident`) because `Some`/`None`/
        // `Ok`/`Err` — used constantly as match-pattern heads across
        // Document 24's examples — are Document 3 keywords, not lexer
        // identifiers. Without this, the most basic `Some(x) => ...` /
        // `None => ...` pattern would fail to parse at all.
        let mut name = self.expect_word()?;
        for seg in self.parse_further_path_segments()? {
            name = format!("{}::{}", name, seg);
        }
        if self.eat_delim(Delim::LParen) {
            let mut pats = Vec::new();
            if !self.check_delim(Delim::RParen) {
                pats.push(self.parse_pattern()?);
                while self.eat_delim(Delim::Comma) {
                    if self.check_delim(Delim::RParen) { break; }
                    pats.push(self.parse_pattern()?);
                }
            }
            self.expect_delim(Delim::RParen)?;
            return Ok(Pattern::TupleStruct(name, pats));
        }
        Ok(Pattern::Ident(name))
    }

    fn parse_literal(&mut self) -> ParseResult<Literal> {
        let t = self.advance();
        Ok(match t.kind {
            TokenKind::Int(s) => Literal::Int(s),
            TokenKind::IntHex(s) => Literal::IntHex(s),
            TokenKind::IntOct(s) => Literal::IntOct(s),
            TokenKind::IntBin(s) => Literal::IntBin(s),
            TokenKind::Float(s) => Literal::Float(s),
            TokenKind::Str(s) => Literal::Str(s),
            TokenKind::RawStr(s) => Literal::RawStr(s),
            TokenKind::Char(s) => Literal::Char(s),
            TokenKind::Bool(b) => Literal::Bool(b),
            TokenKind::Null => Literal::Null,
            other => return self.error(format!("expected literal, found {:?}", other)),
        })
    }

    // ---------- statements & blocks ----------

    fn parse_block(&mut self) -> ParseResult<Block> {
        self.expect_delim(Delim::LBrace)?;
        let mut stmts = Vec::new();
        let mut tail = None;
        while !self.check_delim(Delim::RBrace) && !self.at_end() {
            if self.check_op(Op::Hash) {
                let (kind, items) = self.parse_target_block_items()?;
                let inner_stmts = items.into_iter().map(|i| Stmt::Item(Box::new(i))).collect();
                stmts.push(Stmt::TargetBlock(kind, Block { stmts: inner_stmts, tail: None }));
                continue;
            }
            if item_start_keyword(self.peek_kind()) {
                stmts.push(Stmt::Item(Box::new(self.parse_item()?)));
                continue;
            }
            let stmt = self.parse_stmt()?;
            match stmt {
                StmtOrTail::Stmt(s) => stmts.push(s),
                StmtOrTail::Tail(e) => {
                    tail = Some(Box::new(e));
                    break;
                }
            }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(Block { stmts, tail })
    }

    fn parse_stmt(&mut self) -> ParseResult<StmtOrTail> {
        if self.eat_kw(Keyword::Let) {
            let mutable = self.eat_kw(Keyword::Mut);
            let pattern = self.parse_pattern()?;
            let ty = if self.eat_delim(Delim::Colon) { Some(self.parse_type()?) } else { None };
            let value = if self.eat_op(Op::Eq) { Some(self.parse_expr(0)?) } else { None };
            self.expect_delim(Delim::Semi)?;
            return Ok(StmtOrTail::Stmt(Stmt::Let { mutable, pattern, ty, value }));
        }
        if self.eat_kw(Keyword::Return) {
            let value = if self.check_delim(Delim::Semi) { None } else { Some(self.parse_expr(0)?) };
            self.expect_delim(Delim::Semi)?;
            return Ok(StmtOrTail::Stmt(Stmt::Return(value)));
        }
        if self.eat_kw(Keyword::Break) {
            let value = if self.check_delim(Delim::Semi) { None } else { Some(self.parse_expr(0)?) };
            self.expect_delim(Delim::Semi)?;
            return Ok(StmtOrTail::Stmt(Stmt::Break { label: None, value }));
        }
        if self.eat_kw(Keyword::Continue) {
            self.expect_delim(Delim::Semi)?;
            return Ok(StmtOrTail::Stmt(Stmt::Continue { label: None }));
        }
        if self.check_kw(Keyword::Yield) {
            self.advance();
            let e = self.parse_expr(0)?;
            self.expect_delim(Delim::Semi)?;
            return Ok(StmtOrTail::Stmt(Stmt::Yield(e)));
        }
        let expr = self.parse_expr(0)?;
        if self.eat_delim(Delim::Semi) {
            return Ok(StmtOrTail::Stmt(Stmt::Expr(expr)));
        }
        if self.check_delim(Delim::RBrace) {
            return Ok(StmtOrTail::Tail(expr));
        }
        if is_block_like(&expr) {
            return Ok(StmtOrTail::Stmt(Stmt::Expr(expr)));
        }
        self.error("expected `;` after expression statement")
    }

    // ---------- expressions: Pratt parser ----------

    pub fn parse_expr(&mut self, min_bp: u8) -> ParseResult<Expr> {
        let mut lhs = self.parse_prefix()?;
        let mut last_nonchain_bp: Option<u8> = None;
        loop {
            if self.check_kw(Keyword::As) {
                self.advance();
                let ty = self.parse_type()?;
                lhs = Expr::Cast { expr: Box::new(lhs), ty: Box::new(ty) };
                continue;
            }
            let Some(op) = current_infix_op(self.peek_kind()) else { break };
            let Some((l_bp, r_bp, nonchain)) = infix_binding_power(op) else { break };
            if l_bp < min_bp {
                break;
            }
            if nonchain {
                if last_nonchain_bp == Some(l_bp) {
                    return self.error("chained comparison is not allowed; use `&&` to combine comparisons explicitly");
                }
                last_nonchain_bp = Some(l_bp);
            }
            self.advance();
            let rhs = self.parse_expr(r_bp)?;
            lhs = build_infix(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_prefix(&mut self) -> ParseResult<Expr> {
        if self.check_op(Op::Not) {
            self.advance();
            let e = self.parse_prefix()?;
            return Ok(Expr::Unary { op: UnaryOp::Not, expr: Box::new(e) });
        }
        if self.check_op(Op::Tilde) {
            self.advance();
            let e = self.parse_prefix()?;
            return Ok(Expr::Unary { op: UnaryOp::BitNot, expr: Box::new(e) });
        }
        if self.check_op(Op::Minus) {
            self.advance();
            let e = self.parse_prefix()?;
            return Ok(Expr::Unary { op: UnaryOp::Neg, expr: Box::new(e) });
        }
        if self.check_kw(Keyword::Await) {
            self.advance();
            let e = self.parse_prefix()?;
            return Ok(Expr::Await(Box::new(e)));
        }
        if self.check_kw(Keyword::Borrow) {
            self.advance();
            let mutable = self.eat_kw(Keyword::Mut);
            let e = self.parse_prefix()?;
            return Ok(Expr::Borrow { mutable, expr: Box::new(e) });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> ParseResult<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.check_op(Op::Dot) {
                self.advance();
                let name = self.expect_word()?;
                // optional turbofish on a method call: `.parse::<u64>()`
                // (Document 16 §1.21.1's `"42".parse::<i32>()?`, used
                // directly in Document 24 §1). Parsed and discarded for
                // the same reason as the primary-position turbofish above.
                if self.check_op(Op::ColonColon) {
                    let save = self.pos;
                    self.advance();
                    if self.check_op(Op::Lt) {
                        let _ = self.parse_generic_args_opt()?;
                    } else {
                        self.pos = save;
                    }
                }
                if self.check_delim(Delim::LParen) {
                    let args = self.parse_arg_list()?;
                    expr = Expr::MethodCall { receiver: Box::new(expr), name, args };
                } else {
                    expr = Expr::Field { expr: Box::new(expr), name };
                }
                continue;
            }
            if self.check_op(Op::Question) {
                self.advance();
                expr = Expr::Propagate(Box::new(expr));
                continue;
            }
            if self.check_delim(Delim::LBracket) {
                self.advance();
                let idx = self.parse_expr(0)?;
                self.expect_delim(Delim::RBracket)?;
                expr = Expr::Index { expr: Box::new(expr), index: Box::new(idx) };
                continue;
            }
            if self.check_delim(Delim::LParen) {
                let args = self.parse_arg_list()?;
                expr = Expr::Call { callee: Box::new(expr), args };
                continue;
            }
            if self.check_kw(Keyword::Style) {
                self.advance();
                let props = self.parse_brace_prop_list()?;
                expr = Expr::Styled { expr: Box::new(expr), props };
                continue;
            }
            if self.check_kw(Keyword::Layout) {
                self.advance();
                let props = self.parse_brace_prop_list()?;
                self.expect_delim(Delim::LBrace)?;
                let mut children = Vec::new();
                while !self.check_delim(Delim::RBrace) {
                    children.push(self.parse_expr(0)?);
                    if !self.eat_delim(Delim::Comma) { break; }
                }
                self.expect_delim(Delim::RBrace)?;
                expr = Expr::Layout { expr: Box::new(expr), props, children };
                continue;
            }
            break;
        }
        Ok(expr)
    }

    /// Parses a `{ name: expr, name: expr, ... }` property list, used by
    /// both `style { ... }` and the first brace group of `layout { ... }
    /// { ... }` (Document 18 §7/§7.1).
    fn parse_brace_prop_list(&mut self) -> ParseResult<Vec<(String, Expr)>> {
        self.expect_delim(Delim::LBrace)?;
        let mut props = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            let name = self.expect_word()?;
            self.expect_delim(Delim::Colon)?;
            let value = self.parse_expr(0)?;
            props.push((name, value));
            if !self.eat_delim(Delim::Comma) { break; }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(props)
    }

    fn parse_arg_list(&mut self) -> ParseResult<Vec<Arg>> {
        self.expect_delim(Delim::LParen)?;
        let mut args = Vec::new();
        if !self.check_delim(Delim::RParen) {
            args.push(self.parse_arg()?);
            while self.eat_delim(Delim::Comma) {
                if self.check_delim(Delim::RParen) { break; }
                args.push(self.parse_arg()?);
            }
        }
        self.expect_delim(Delim::RParen)?;
        Ok(args)
    }

    fn parse_arg(&mut self) -> ParseResult<Arg> {
        if self.check_ident() || matches!(self.peek_kind(), TokenKind::Keyword(_)) {
            let save = self.pos;
            if let Ok(name) = self.expect_word() {
                if self.check_delim(Delim::Colon) {
                    self.advance();
                    let value = if name == "on" {
                        self.parse_event_handler_or_expr()?
                    } else {
                        self.parse_expr(0)?
                    };
                    return Ok(Arg { name: Some(name), value });
                }
            }
            self.pos = save;
        }
        Ok(Arg { name: None, value: self.parse_expr(0)? })
    }

    /// Document 18 §5: `on: <eventName> => <expression or closure>`.
    /// Narrowly scoped to the `on:` argument label (not general
    /// expression grammar) — detected by a 2-token lookahead (`word`
    /// immediately followed by `=>`) so it doesn't interfere with an
    /// ordinary expression ever being passed as an `on:` value in some
    /// other form.
    fn parse_event_handler_or_expr(&mut self) -> ParseResult<Expr> {
        let looks_like_event_arrow = matches!(self.peek_kind(), TokenKind::Ident(_) | TokenKind::Keyword(_))
            && matches!(self.peek_at(1), TokenKind::Op(Op::FatArrow));
        if looks_like_event_arrow {
            let event = self.expect_word()?;
            self.expect_op(Op::FatArrow)?;
            let body = self.parse_expr(0)?;
            return Ok(Expr::EventHandler { event, body: Box::new(body) });
        }
        self.parse_expr(0)
    }

    fn parse_primary(&mut self) -> ParseResult<Expr> {
        if matches!(self.peek_kind(), TokenKind::Int(_) | TokenKind::Float(_) | TokenKind::Str(_)
            | TokenKind::RawStr(_) | TokenKind::Char(_) | TokenKind::Bool(_) | TokenKind::Null
            | TokenKind::IntHex(_) | TokenKind::IntOct(_) | TokenKind::IntBin(_)) {
            return Ok(Expr::Literal(self.parse_literal()?));
        }
        if self.check_kw(Keyword::If) {
            return Ok(Expr::If(Box::new(self.parse_if_expr()?)));
        }
        if self.check_kw(Keyword::Match) {
            return Ok(Expr::Match(Box::new(self.parse_match_expr()?)));
        }
        if self.check_kw(Keyword::Loop) || self.check_kw(Keyword::While)
            || self.check_kw(Keyword::For) || self.check_kw(Keyword::Do) {
            return Ok(Expr::Loop(Box::new(self.parse_loop_expr()?)));
        }
        if self.check_kw(Keyword::Spawn) {
            self.advance();
            let is_move = self.eat_kw(Keyword::Move);
            let body = if self.check_delim(Delim::LBrace) {
                SpawnBody::Block(self.parse_block()?)
            } else {
                SpawnBody::Closure(Box::new(self.parse_closure_expr(false)?))
            };
            return Ok(Expr::Spawn { is_move, body });
        }
        if self.check_kw(Keyword::Select) {
            return self.parse_select_expr();
        }
        if self.check_kw(Keyword::Query) {
            return Ok(Expr::Query(self.parse_query_expr()?));
        }
        if self.check_kw(Keyword::Try) {
            return self.parse_try_catch_expr();
        }
        if self.check_kw(Keyword::Throw) {
            self.advance();
            let e = self.parse_expr(0)?;
            return Ok(Expr::Throw(Box::new(e)));
        }
        if self.check_kw(Keyword::Return) {
            self.advance();
            let e = if self.check_delim(Delim::Semi) || self.check_delim(Delim::RBrace)
                || self.check_delim(Delim::Comma) {
                None
            } else {
                Some(Box::new(self.parse_expr(0)?))
            };
            return Ok(Expr::Return(e));
        }
        if self.check_kw(Keyword::Move) || self.check_op(Op::Pipe) || self.check_op(Op::OrOr) {
            let is_move = self.eat_kw(Keyword::Move);
            return Ok(Expr::Closure(Box::new(self.parse_closure_expr(is_move)?)));
        }
        if self.check_kw(Keyword::Unsafe) {
            self.advance();
            return Ok(Expr::Unsafe(Box::new(self.parse_block()?)));
        }
        if self.check_delim(Delim::LBrace) {
            return Ok(Expr::Block(Box::new(self.parse_block()?)));
        }
        if self.check_delim(Delim::LBracket) {
            self.advance();
            let mut items = Vec::new();
            if !self.check_delim(Delim::RBracket) {
                items.push(self.parse_expr(0)?);
                while self.eat_delim(Delim::Comma) {
                    if self.check_delim(Delim::RBracket) { break; }
                    items.push(self.parse_expr(0)?);
                }
            }
            self.expect_delim(Delim::RBracket)?;
            return Ok(Expr::Array(items));
        }
        if self.check_delim(Delim::LParen) {
            self.advance();
            if self.eat_delim(Delim::RParen) {
                return Ok(Expr::Tuple(Vec::new()));
            }
            let mut items = vec![self.parse_expr(0)?];
            let mut is_tuple = false;
            while self.eat_delim(Delim::Comma) {
                is_tuple = true;
                if self.check_delim(Delim::RParen) { break; }
                items.push(self.parse_expr(0)?);
            }
            self.expect_delim(Delim::RParen)?;
            return Ok(if is_tuple {
                Expr::Tuple(items)
            } else {
                Expr::Paren(Box::new(items.into_iter().next().unwrap()))
            });
        }
        // By this point every keyword with dedicated expression-starting
        // behavior (if/match/loop/while/for/do/spawn/select/query/try/
        // throw/move/return) has already been checked and consumed
        // above. Any keyword token reaching this point is being used as
        // an ordinary word — e.g. `server::Http::bind(...)` (Document
        // 24 §1) or `TextDecoration::None` (Document 24 §2) — so it's
        // safe to accept it here via `expect_word` rather than requiring
        // a plain identifier. Without this, any expression starting with
        // a domain keyword used as a namespace/path segment would fail
        // to parse at all.
        if self.check_ident() || self.check_kw(Keyword::SelfValue)
            || matches!(self.peek_kind(), TokenKind::Keyword(_)) {
            let name = if self.check_kw(Keyword::SelfValue) {
                self.advance();
                "self".to_string()
            } else {
                self.expect_word()?
            };
            let mut path = vec![name.clone()];
            path.extend(self.parse_further_path_segments()?);
            let base = if path.len() > 1 {
                Expr::Path(path)
            } else {
                Expr::Ident(name)
            };
            // Optional turbofish `::<T, U>` (Document 8 §2's
            // `largest::<i32>(nums)`). Parsed and discarded for now —
            // Phase 2 produces an untyped AST (Document 17 §3); explicit
            // generic-argument tracking at call sites is Phase 5
            // (Generics) territory. This only needs to not choke on the
            // syntax, which it now doesn't.
            if self.check_op(Op::ColonColon) {
                let save = self.pos;
                self.advance();
                if self.check_op(Op::Lt) {
                    let _ = self.parse_generic_args_opt()?;
                } else {
                    self.pos = save;
                }
            }
            if self.check_delim(Delim::LBrace) && self.struct_lit_allowed {
                if self.looks_like_struct_lit_fields() {
                    return self.parse_struct_lit_tail(base);
                }
                return self.parse_component_children_tail(base);
            }
            return Ok(base);
        }
        self.error(format!("expected expression, found {:?}", self.peek_kind()))
    }

    /// Disambiguates `Name { ... }` between a struct literal (`Name {
    /// field: expr, ... }`) and a UI children-list call (`Name { child,
    /// child, ... }`, Document 18's `Column { Text("Left"), Text(...) }`
    /// pattern). `self.peek()` is the `{` itself (not yet consumed) when
    /// this is called. Struct literals always start with `word :`
    /// (field label then colon, per Document 23's `struct_literal`
    /// grammar, which requires at least one `IDENT ":" expr` before any
    /// optional trailing spread); anything else — including an empty
    /// `{}` — is treated as a children list.
    fn looks_like_struct_lit_fields(&self) -> bool {
        let first_is_word = matches!(self.peek_at(1), TokenKind::Ident(_) | TokenKind::Keyword(_));
        let second_is_colon = matches!(self.peek_at(2), TokenKind::Delim(Delim::Colon));
        first_is_word && second_is_colon
    }

    fn parse_component_children_tail(&mut self, base: Expr) -> ParseResult<Expr> {
        let name = match &base {
            Expr::Ident(s) => s.clone(),
            Expr::Path(p) => p.join("::"),
            _ => return self.error("component-children name must be a plain identifier or path"),
        };
        self.expect_delim(Delim::LBrace)?;
        let mut children = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            children.push(self.parse_expr(0)?);
            if !self.eat_delim(Delim::Comma) { break; }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(Expr::ComponentChildren { name, children })
    }

    fn parse_struct_lit_tail(&mut self, base: Expr) -> ParseResult<Expr> {
        let name = match &base {
            Expr::Ident(s) => s.clone(),
            Expr::Path(p) => p.join("::"),
            _ => return self.error("struct literal name must be a plain identifier or path"),
        };
        self.advance(); // '{'
        let mut fields = Vec::new();
        let mut spread = None;
        while !self.check_delim(Delim::RBrace) {
            if self.eat_op(Op::DotDot) {
                spread = Some(Box::new(self.parse_expr(0)?));
                break;
            }
            let fname = self.expect_word()?;
            self.expect_delim(Delim::Colon)?;
            let fval = self.parse_expr(0)?;
            fields.push((fname, fval));
            if !self.eat_delim(Delim::Comma) { break; }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(Expr::StructLit { name, fields, spread })
    }

    fn parse_if_expr(&mut self) -> ParseResult<IfExpr> {
        self.expect_kw(Keyword::If)?;
        let cond = self.parse_expr_no_struct_lit(0)?;
        let then_block = self.parse_block()?;
        let else_branch = if self.eat_kw(Keyword::Else) {
            if self.check_kw(Keyword::If) {
                Some(ElseBranch::If(Box::new(self.parse_if_expr()?)))
            } else {
                Some(ElseBranch::Block(self.parse_block()?))
            }
        } else {
            None
        };
        Ok(IfExpr { cond, then_block, else_branch })
    }

    /// Parses an expression with struct-literal parsing suppressed —
    /// needed for `if`/`match`/`for`/`while` conditions/scrutinees so
    /// `if x { ... }` isn't misparsed as `if (x { }) { ... }`. This
    /// ambiguity is inherent to the grammar as written in Document 23
    /// (which doesn't special-case it) and is resolved here the same
    /// way Rust resolves the identical ambiguity in its own grammar.
    fn parse_expr_no_struct_lit(&mut self, min_bp: u8) -> ParseResult<Expr> {
        let prev = self.struct_lit_allowed;
        self.struct_lit_allowed = false;
        let r = self.parse_expr(min_bp);
        self.struct_lit_allowed = prev;
        r
    }

    fn parse_match_expr(&mut self) -> ParseResult<MatchExpr> {
        self.expect_kw(Keyword::Match)?;
        let scrutinee = self.parse_expr_no_struct_lit(0)?;
        self.expect_delim(Delim::LBrace)?;
        let mut arms = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            let mut patterns = vec![self.parse_pattern()?];
            while self.eat_op(Op::Pipe) {
                patterns.push(self.parse_pattern()?);
            }
            let guard = if self.eat_kw(Keyword::If) { Some(self.parse_expr(0)?) } else { None };
            self.expect_op(Op::FatArrow)?;
            let body = if self.check_delim(Delim::LBrace) {
                MatchArmBody::Block(self.parse_block()?)
            } else {
                MatchArmBody::Expr(Box::new(self.parse_expr(0)?))
            };
            arms.push(MatchArm { patterns, guard, body });
            if !self.eat_delim(Delim::Comma) {
                if self.check_delim(Delim::RBrace) { break; }
            }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(MatchExpr { scrutinee, arms })
    }

    fn parse_loop_expr(&mut self) -> ParseResult<LoopExpr> {
        if self.eat_kw(Keyword::Loop) {
            let body = self.parse_block()?;
            return Ok(LoopExpr::Loop { label: None, body });
        }
        if self.eat_kw(Keyword::While) {
            let cond = self.parse_expr_no_struct_lit(0)?;
            let body = self.parse_block()?;
            return Ok(LoopExpr::While { label: None, cond: Box::new(cond), body });
        }
        if self.eat_kw(Keyword::For) {
            let pattern = self.parse_pattern()?;
            self.expect_kw(Keyword::In)?;
            let iter = self.parse_expr_no_struct_lit(0)?;
            let body = self.parse_block()?;
            return Ok(LoopExpr::For { label: None, pattern, iter: Box::new(iter), body });
        }
        if self.eat_kw(Keyword::Do) {
            let body = self.parse_block()?;
            self.expect_kw(Keyword::While)?;
            self.expect_delim(Delim::LParen)?;
            let cond = self.parse_expr(0)?;
            self.expect_delim(Delim::RParen)?;
            self.expect_delim(Delim::Semi)?;
            return Ok(LoopExpr::DoWhile { body, cond: Box::new(cond) });
        }
        self.error("expected loop/while/for/do")
    }

    fn parse_closure_expr(&mut self, is_move: bool) -> ParseResult<ClosureExpr> {
        let mut params = Vec::new();
        if self.eat_op(Op::OrOr) {
            // `||` lexes as one token: zero-parameter closure
        } else {
            self.expect_op(Op::Pipe)?;
            if !self.check_op(Op::Pipe) {
                loop {
                    // parse_pattern_single, NOT parse_pattern: the latter
                    // also checks for a trailing '|' to build an
                    // or-pattern (`Pattern::Or`), which would misfire on
                    // the closure's own closing '|' delimiter here (e.g.
                    // `|(input, _)| body` -- after parsing `(input, _)`,
                    // the very next token IS a bare '|', but it's the
                    // parameter list's closer, not an or-pattern
                    // separator). Or-patterns aren't shown anywhere in
                    // closure-parameter position in the spec, and would
                    // be structurally ambiguous with the closing pipe
                    // regardless, so this scopes to single patterns only
                    // while still reusing the same underlying parser
                    // used by fn params/let/match.
                    let pattern = self.parse_pattern_single()?;
                    let ty = if self.eat_delim(Delim::Colon) { Some(self.parse_type()?) } else { None };
                    params.push((pattern, ty));
                    if !self.eat_delim(Delim::Comma) { break; }
                }
            }
            self.expect_op(Op::Pipe)?;
        }
        let return_type = if self.eat_op(Op::Arrow) { Some(self.parse_type()?) } else { None };
        let body = if self.check_delim(Delim::LBrace) {
            ClosureBody::Block(self.parse_block()?)
        } else {
            ClosureBody::Expr(Box::new(self.parse_expr(0)?))
        };
        Ok(ClosureExpr { is_move, params, return_type, body })
    }

    fn parse_select_expr(&mut self) -> ParseResult<Expr> {
        self.expect_kw(Keyword::Select)?;
        self.expect_delim(Delim::LBrace)?;
        let mut arms = Vec::new();
        while !self.check_delim(Delim::RBrace) {
            self.expect_kw(Keyword::Case)?;
            let pattern = self.parse_pattern()?;
            self.expect_op(Op::Eq)?;
            // '<-' is lexed as two tokens: Lt then Minus (no combined
            // token for it in the Phase 1 lexer -- Document 2 doesn't
            // list '<-' among its reserved symbols at all; only
            // Document 3/12's inline `select` example uses it). Accept
            // that two-token spelling explicitly here.
            self.expect_op(Op::Lt)?;
            self.expect_op(Op::Minus)?;
            let channel_expr = self.parse_expr(0)?;
            self.expect_delim(Delim::Colon)?;
            let body = self.parse_expr(0)?;
            arms.push(SelectArm { pattern, channel_expr, body });
            if !self.eat_delim(Delim::Comma) { break; }
        }
        self.expect_delim(Delim::RBrace)?;
        Ok(Expr::Select(arms))
    }

    fn parse_query_expr(&mut self) -> ParseResult<QueryExpr> {
        self.expect_kw(Keyword::Query)?;
        let table = self.expect_word()?;
        let mut clauses = Vec::new();
        loop {
            if self.eat_kw(Keyword::Where) {
                clauses.push(QueryClause::Where(Box::new(self.parse_expr(0)?)));
            } else if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "orderBy") {
                self.advance();
                let col = self.expect_word()?;
                clauses.push(QueryClause::OrderBy(col));
            } else if self.eat_kw(Keyword::Insert) {
                clauses.push(QueryClause::Insert(Box::new(self.parse_expr(0)?)));
            } else if self.eat_kw(Keyword::Update) {
                self.expect_delim(Delim::LBrace)?;
                let mut fields = Vec::new();
                while !self.check_delim(Delim::RBrace) {
                    let fname = self.expect_word()?;
                    self.expect_delim(Delim::Colon)?;
                    let fval = self.parse_expr(0)?;
                    fields.push((fname, fval));
                    if !self.eat_delim(Delim::Comma) { break; }
                }
                self.expect_delim(Delim::RBrace)?;
                clauses.push(QueryClause::Update(fields));
            } else if self.eat_kw(Keyword::Delete) {
                clauses.push(QueryClause::Delete);
            } else if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "first") {
                self.advance();
                clauses.push(QueryClause::First);
            } else if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "count") {
                self.advance();
                self.expect_delim(Delim::LParen)?;
                self.expect_delim(Delim::RParen)?;
                clauses.push(QueryClause::Count);
            } else if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "join") {
                self.advance();
                let jt = self.expect_word()?;
                self.expect_kw(Keyword::On)?;
                let on = self.parse_expr(0)?;
                clauses.push(QueryClause::Join { table: jt, on: Box::new(on) });
            } else {
                break;
            }
        }
        Ok(QueryExpr { table, clauses })
    }

    fn parse_try_catch_expr(&mut self) -> ParseResult<Expr> {
        self.expect_kw(Keyword::Try)?;
        let try_block = self.parse_block()?;
        self.expect_kw(Keyword::Catch)?;
        self.expect_delim(Delim::LParen)?;
        let catch_var = self.expect_word()?;
        self.expect_delim(Delim::RParen)?;
        let catch_block = self.parse_block()?;
        Ok(Expr::TryCatch { try_block, catch_var, catch_block })
    }
}

enum StmtOrTail {
    Stmt(Stmt),
    Tail(Expr),
}

fn is_block_like(e: &Expr) -> bool {
    matches!(e, Expr::If(_) | Expr::Match(_) | Expr::Loop(_) | Expr::Block(_)
        | Expr::Spawn { .. } | Expr::Select(_) | Expr::TryCatch { .. } | Expr::Unsafe(_))
}

fn is_primitive_type_name(s: &str) -> bool {
    matches!(s,
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" |
        "u8" | "u16" | "u32" | "u64" | "u128" | "usize" |
        "f32" | "f64" | "bool" | "char" | "String" | "str")
}

fn item_start_keyword(kind: &TokenKind) -> bool {
    matches!(kind, TokenKind::Keyword(k) if matches!(k,
        Keyword::Fn | Keyword::Async | Keyword::Struct | Keyword::Enum | Keyword::Trait
        | Keyword::Impl | Keyword::Mod | Keyword::Use | Keyword::Import | Keyword::Const
        | Keyword::Static | Keyword::Type | Keyword::Table | Keyword::Index | Keyword::Schema
        | Keyword::Ui | Keyword::Component | Keyword::Server | Keyword::Actor | Keyword::Pub
    )) || matches!(kind, TokenKind::Op(Op::At) | TokenKind::Op(Op::Hash))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum InfixOp {
    Plus, Minus, Star, Slash, Percent, StarStar,
    EqEq, NotEq, Lt, Gt, LtEq, GtEq,
    AndAnd, OrOr, QuestionQuestion,
    Amp, Pipe, Caret, Shl, Shr,
    DotDot, DotDotEq,
    Assign(AssignOp),
}

fn current_infix_op(kind: &TokenKind) -> Option<InfixOp> {
    let TokenKind::Op(op) = kind else { return None };
    Some(match op {
        Op::Plus => InfixOp::Plus, Op::Minus => InfixOp::Minus,
        Op::Star => InfixOp::Star, Op::Slash => InfixOp::Slash,
        Op::Percent => InfixOp::Percent, Op::StarStar => InfixOp::StarStar,
        Op::EqEq => InfixOp::EqEq, Op::NotEq => InfixOp::NotEq,
        Op::Lt => InfixOp::Lt, Op::Gt => InfixOp::Gt,
        Op::LtEq => InfixOp::LtEq, Op::GtEq => InfixOp::GtEq,
        Op::AndAnd => InfixOp::AndAnd, Op::OrOr => InfixOp::OrOr,
        Op::QuestionQuestion => InfixOp::QuestionQuestion,
        Op::Amp => InfixOp::Amp, Op::Pipe => InfixOp::Pipe, Op::Caret => InfixOp::Caret,
        Op::Shl => InfixOp::Shl, Op::Shr => InfixOp::Shr,
        Op::DotDot => InfixOp::DotDot, Op::DotDotEq => InfixOp::DotDotEq,
        Op::Eq => InfixOp::Assign(AssignOp::Eq),
        Op::PlusEq => InfixOp::Assign(AssignOp::PlusEq), Op::MinusEq => InfixOp::Assign(AssignOp::MinusEq),
        Op::StarEq => InfixOp::Assign(AssignOp::StarEq), Op::SlashEq => InfixOp::Assign(AssignOp::SlashEq),
        Op::PercentEq => InfixOp::Assign(AssignOp::PercentEq), Op::AmpEq => InfixOp::Assign(AssignOp::AmpEq),
        Op::PipeEq => InfixOp::Assign(AssignOp::PipeEq), Op::CaretEq => InfixOp::Assign(AssignOp::CaretEq),
        Op::ShlEq => InfixOp::Assign(AssignOp::ShlEq), Op::ShrEq => InfixOp::Assign(AssignOp::ShrEq),
        _ => return None,
    })
}

/// Returns (left_bp, right_bp, is_nonchaining). Verified against
/// Document 4 §10/§11 via the Python prototype (26/26 cases) before
/// this port; see PROGRESS.md Phase 2.
fn infix_binding_power(op: InfixOp) -> Option<(u8, u8, bool)> {
    use InfixOp::*;
    Some(match op {
        Assign(_) => (2, 1, false), // right-assoc
        QuestionQuestion => (3, 4, false),
        OrOr => (5, 6, false),
        AndAnd => (7, 8, false),
        EqEq | NotEq => (9, 10, true),
        Lt | Gt | LtEq | GtEq => (10, 11, true),
        DotDot | DotDotEq => (11, 12, true),
        Pipe => (12, 13, false),
        Caret => (14, 15, false),
        Amp => (16, 17, false),
        Shl | Shr => (18, 19, false),
        Plus | Minus => (20, 21, false),
        Star | Slash | Percent => (22, 23, false),
        StarStar => (25, 24, false), // right-assoc
    })
}

fn build_infix(op: InfixOp, lhs: Expr, rhs: Expr) -> Expr {
    use InfixOp::*;
    if let Assign(aop) = op {
        return Expr::Assign { op: aop, lhs: Box::new(lhs), rhs: Box::new(rhs) };
    }
    if matches!(op, DotDot | DotDotEq) {
        return Expr::Range { lo: Box::new(lhs), hi: Box::new(rhs), inclusive: matches!(op, DotDotEq) };
    }
    let bop = match op {
        Plus => BinaryOp::Add, Minus => BinaryOp::Sub, Star => BinaryOp::Mul,
        Slash => BinaryOp::Div, Percent => BinaryOp::Mod, StarStar => BinaryOp::Pow,
        EqEq => BinaryOp::EqEq, NotEq => BinaryOp::NotEq, Lt => BinaryOp::Lt,
        Gt => BinaryOp::Gt, LtEq => BinaryOp::LtEq, GtEq => BinaryOp::GtEq,
        AndAnd => BinaryOp::AndAnd, OrOr => BinaryOp::OrOr,
        QuestionQuestion => BinaryOp::Coalesce,
        Amp => BinaryOp::BitAnd, Pipe => BinaryOp::BitOr, Caret => BinaryOp::BitXor,
        Shl => BinaryOp::Shl, Shr => BinaryOp::Shr,
        DotDot | DotDotEq | Assign(_) => unreachable!(),
    };
    Expr::Binary { op: bop, lhs: Box::new(lhs), rhs: Box::new(rhs) }
}

/// Parses a full program from a token stream. Convenience entry point.
pub fn parse_program(tokens: Vec<Token>) -> (Program, Vec<ParseError>) {
    let mut p = Parser::new(tokens);
    let prog = p.parse_program();
    (prog, p.errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer;

    /// Renders an expression as a compact s-expression string, mirroring
    /// the Python prototype's `to_sexpr` helper exactly, so the same 26
    /// test cases verified there (Document 4 §10/§11) can be checked
    /// against the real Rust parser output with identical expected
    /// strings.
    fn to_sexpr(e: &Expr) -> String {
        match e {
            Expr::Literal(Literal::Int(s)) | Expr::Literal(Literal::Float(s)) => s.clone(),
            Expr::Ident(s) => s.clone(),
            Expr::Paren(inner) => to_sexpr(inner),
            Expr::Binary { op, lhs, rhs } => format!("({} {} {})", binop_sym(*op), to_sexpr(lhs), to_sexpr(rhs)),
            Expr::Assign { op, lhs, rhs } => format!("({} {} {})", assignop_sym(*op), to_sexpr(lhs), to_sexpr(rhs)),
            Expr::Unary { op, expr } => format!("({} {})", unop_sym(*op), to_sexpr(expr)),
            Expr::Cast { expr, ty } => format!("(as {} {})", to_sexpr(expr), ty_sym(ty)),
            Expr::Propagate(inner) => format!("(? {})", to_sexpr(inner)),
            Expr::Field { expr, name } => format!("(. {} {})", to_sexpr(expr), name),
            Expr::Call { callee, args } => format!(
                "(call {} [{}])",
                to_sexpr(callee),
                args.iter().map(|a| to_sexpr(&a.value)).collect::<Vec<_>>().join(", ")
            ),
            Expr::MethodCall { receiver, name, args } => format!(
                "(methodcall {} {} [{}])",
                to_sexpr(receiver),
                name,
                args.iter().map(|a| to_sexpr(&a.value)).collect::<Vec<_>>().join(", ")
            ),
            Expr::Index { expr, index } => format!("(index {} {})", to_sexpr(expr), to_sexpr(index)),
            Expr::Range { lo, hi, inclusive } => format!("({} {} {})", if *inclusive { ".." } else { ".." }, to_sexpr(lo), to_sexpr(hi)),
            other => format!("{:?}", other),
        }
    }

    fn binop_sym(op: BinaryOp) -> &'static str {
        use BinaryOp::*;
        match op {
            Add => "+", Sub => "-", Mul => "*", Div => "/", Mod => "%", Pow => "**",
            EqEq => "==", NotEq => "!=", Lt => "<", Gt => ">", LtEq => "<=", GtEq => ">=",
            AndAnd => "&&", OrOr => "||", BitAnd => "&", BitOr => "|", BitXor => "^",
            Shl => "<<", Shr => ">>", Coalesce => "??",
        }
    }
    fn assignop_sym(op: AssignOp) -> &'static str {
        use AssignOp::*;
        match op {
            Eq => "=", PlusEq => "+=", MinusEq => "-=", StarEq => "*=", SlashEq => "/=",
            PercentEq => "%=", AmpEq => "&=", PipeEq => "|=", CaretEq => "^=",
            ShlEq => "<<=", ShrEq => ">>=",
        }
    }
    fn unop_sym(op: UnaryOp) -> &'static str {
        match op { UnaryOp::Not => "!", UnaryOp::BitNot => "~", UnaryOp::Neg => "-" }
    }
    fn ty_sym(t: &Type) -> String {
        match t {
            Type::Primitive(s) => s.clone(),
            Type::Named(s, _) => s.clone(),
            other => format!("{:?}", other),
        }
    }

    fn parse_expr_str(src: &str) -> Expr {
        let (tokens, lex_errs) = lexer::tokenize(src);
        assert!(lex_errs.is_empty(), "lex errors: {:?}", lex_errs);
        let mut p = Parser::new(tokens);
        let e = p.parse_expr(0).expect("expected successful parse");
        assert!(p.at_end(), "leftover tokens after expression");
        e
    }

    fn expect_parse_error(src: &str) {
        let (tokens, lex_errs) = lexer::tokenize(src);
        assert!(lex_errs.is_empty());
        let mut p = Parser::new(tokens);
        let r = p.parse_expr(0);
        assert!(r.is_err(), "expected a parse error for `{}`, got {:?}", src, r);
    }

    // ---- Document 4 §11's exact verification-pass cases ----

    #[test]
    fn doc4_case_add_before_mul() {
        assert_eq!(to_sexpr(&parse_expr_str("a + b * c")), "(+ a (* b c))");
    }
    #[test]
    fn doc4_case_cast_binds_tighter_than_add() {
        assert_eq!(to_sexpr(&parse_expr_str("a as f64 + b")), "(+ (as a f64) b)");
    }
    #[test]
    fn doc4_case_assign_evaluates_after_add() {
        assert_eq!(to_sexpr(&parse_expr_str("x = y + z")), "(= x (+ y z))");
    }
    #[test]
    fn doc4_case_and_before_or() {
        assert_eq!(to_sexpr(&parse_expr_str("a && b || c")), "(|| (&& a b) c)");
    }
    #[test]
    fn doc4_case_postfix_chain_propagate_then_field() {
        assert_eq!(to_sexpr(&parse_expr_str("result?.field")), "(. (? result) field)");
    }

    // ---- Full Document 4 §10 table, row by row (26 cases total,
    //      matching the executed Python prototype 1:1) ----

    #[test] fn row1_vs_2_unary_before_mul() {
        assert_eq!(to_sexpr(&parse_expr_str("-a * b")), "(* (- a) b)");
    }
    #[test] fn row2_vs_3_unary_before_pow() {
        assert_eq!(to_sexpr(&parse_expr_str("-a ** b")), "(** (- a) b)");
    }
    #[test] fn row3_pow_right_assoc() {
        assert_eq!(to_sexpr(&parse_expr_str("a ** b ** c")), "(** a (** b c))");
    }
    #[test] fn row4_vs_5_mul_before_add() {
        assert_eq!(to_sexpr(&parse_expr_str("a + b * c - d")), "(- (+ a (* b c)) d)");
    }
    #[test] fn row5_add_left_assoc() {
        assert_eq!(to_sexpr(&parse_expr_str("a - b - c")), "(- (- a b) c)");
    }
    #[test] fn row6_vs_5_shift_after_add() {
        assert_eq!(to_sexpr(&parse_expr_str("a + b << c")), "(<< (+ a b) c)");
    }
    #[test] fn row7_vs_6_and_after_shift() {
        assert_eq!(to_sexpr(&parse_expr_str("a << b & c")), "(& (<< a b) c)");
    }
    #[test] fn row8_vs_7_xor_after_and() {
        assert_eq!(to_sexpr(&parse_expr_str("a & b ^ c")), "(^ (& a b) c)");
    }
    #[test] fn row9_vs_8_or_after_xor() {
        assert_eq!(to_sexpr(&parse_expr_str("a ^ b | c")), "(| (^ a b) c)");
    }
    #[test] fn row10_vs_9_range_before_or_groups() {
        assert_eq!(to_sexpr(&parse_expr_str("a | b .. c | d")), "(.. (| a b) (| c d))");
    }
    #[test] fn row11_vs_10_cmp_before_range_groups() {
        assert_eq!(to_sexpr(&parse_expr_str("a .. b < c .. d")), "(< (.. a b) (.. c d))");
    }
    #[test] fn row12_vs_11_eq_after_cmp_groups() {
        assert_eq!(to_sexpr(&parse_expr_str("a < b == c < d")), "(== (< a b) (< c d))");
    }
    #[test] fn row13_vs_12_and_after_eq_groups() {
        assert_eq!(to_sexpr(&parse_expr_str("a == b && c == d")), "(&& (== a b) (== c d))");
    }
    #[test] fn row14_vs_13_or_after_and_groups() {
        assert_eq!(to_sexpr(&parse_expr_str("a && b || c && d")), "(|| (&& a b) (&& c d))");
    }
    #[test] fn row15_vs_14_coalesce_after_or_groups() {
        assert_eq!(to_sexpr(&parse_expr_str("a || b ?? c || d")), "(?? (|| a b) (|| c d))");
    }
    #[test] fn row17_assign_right_assoc() {
        assert_eq!(to_sexpr(&parse_expr_str("a = b = c")), "(= a (= b c))");
    }
    #[test] fn nonchaining_comparison_rejected() {
        expect_parse_error("a < b < c");
    }
    #[test] fn call_binds_before_binary() {
        assert_eq!(to_sexpr(&parse_expr_str("f(x) + 1")), "(+ (call f [x]) 1)");
    }
    #[test] fn index_binds_before_binary() {
        assert_eq!(to_sexpr(&parse_expr_str("a[0] + 1")), "(+ (index a 0) 1)");
    }
    #[test] fn member_access_chain() {
        assert_eq!(to_sexpr(&parse_expr_str("a.b.c")), "(. (. a b) c)");
    }
    #[test] fn call_then_field() {
        // a.b(1) is a single MethodCall node (receiver=a, name=b, args=[1]),
        // not Field(a,b) generically Call'd -- Document 23's postfix_op
        // grammar combines ".IDENT" with an optional "(args)" as ONE
        // production, and the parser correctly builds one MethodCall
        // node when the parens are present. The original expected value
        // here ("(call (. a b) [1])") was carried over from the Python
        // precedence-prototype, which never modeled method calls as a
        // distinct case (it only needed to validate precedence/
        // associativity, not full postfix semantics) -- it's the
        // prototype's simplification that was wrong here, not this
        // parser. Corrected to match what the AST is actually supposed
        // to produce.
        assert_eq!(to_sexpr(&parse_expr_str("a.b(1).c")), "(. (methodcall a b [1]) c)");
    }

    // ---- Structural sanity: does the parser accept a realistic
    //      multi-construct snippet without error? (Deep structural
    //      round-trip assertions live in tests/parser_doc24.rs.) ----

    #[test]
    fn parses_full_function_with_control_flow() {
        let src = r#"
fn largest<T>(list: [T]) -> T where T: Comparable {
    let mut max = list[0];
    for item in list {
        if item.compareTo(borrow max) > 0 {
            max = item;
        }
    }
    return max;
}
"#;
        let (tokens, lex_errs) = lexer::tokenize(src);
        assert!(lex_errs.is_empty());
        let (_prog, errs) = parse_program(tokens);
        assert!(errs.is_empty(), "parse errors: {:?}", errs);
    }

    #[test]
    fn parser_recovers_from_one_bad_item_and_keeps_going() {
        // Document 25 Phase-1 error-recovery precedent extended to the
        // parser: one malformed item shouldn't prevent the rest of the
        // file from parsing.
        let src = r#"
fn good1() { let x = 1; }
struct !!! broken
fn good2() { let y = 2; }
"#;
        let (tokens, _lex_errs) = lexer::tokenize(src);
        let (prog, errs) = parse_program(tokens);
        assert!(!errs.is_empty(), "expected at least one recorded parse error");
        let fn_names: Vec<&str> = prog.items.iter().filter_map(|i| match &i.kind {
            ItemKind::Fn(f) => Some(f.name.as_str()),
            _ => None,
        }).collect();
        assert!(fn_names.contains(&"good1"), "good1 should have parsed despite the broken item: {:?}", fn_names);
        assert!(fn_names.contains(&"good2"), "good2 should have parsed despite the broken item: {:?}", fn_names);
    }
}
