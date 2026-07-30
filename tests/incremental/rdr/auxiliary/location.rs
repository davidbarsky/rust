#![feature(proc_macro_quote, proc_macro_span)]

extern crate proc_macro;

use proc_macro::{Ident, Literal, Span, TokenStream, TokenTree};

#[proc_macro]
pub fn location(input: TokenStream) -> TokenStream {
    let span = input.into_iter().next().expect("input token").span();
    TokenTree::Literal(Literal::u32_unsuffixed(span.line() as u32)).into()
}

#[proc_macro]
pub fn observe_spans(input: TokenStream) -> TokenStream {
    let _ = inspect_span_apis(input);
    TokenTree::Literal(Literal::u32_unsuffixed(1)).into()
}

#[proc_macro]
pub fn export_span_observations(input: TokenStream) -> TokenStream {
    TokenTree::Literal(Literal::u32_unsuffixed(inspect_span_apis(input))).into()
}

fn inspect_span_apis(input: TokenStream) -> u32 {
    let literal = match input.into_iter().next().expect("input token") {
        TokenTree::Literal(literal) => literal,
        TokenTree::Group(_) => panic!("expected literal"),
        TokenTree::Ident(_) => panic!("expected literal"),
        TokenTree::Punct(_) => panic!("expected literal"),
    };
    let span = literal.span();
    let start = span.start();
    let end = span.end();
    let source = span.source();
    let interned = Ident::new("interned", span).span();
    let _ = proc_macro::quote!(let quoted = 1;);

    let _ = format!("{span:?}");
    let _ = span.parent();
    let _ = span.resolved_at(source);
    let _ = span.located_at(source);
    let _ = span.eq(&interned);
    let _ = span.file();
    let _ = span.local_file();
    let _ = span.byte_range();
    let _ = span.line();
    let _ = span.column();
    let _ = span.join(end);
    let _ = start.join(span);
    let _ = literal.subspan(0..1);
    let _ = span.source_text();

    span.line() as u32
}

#[proc_macro]
pub fn define_hygiene(input: TokenStream) -> TokenStream {
    let hygiene = input.to_string();
    let span = if hygiene == "call" {
        Span::call_site()
    } else if hygiene == "mixed" {
        Span::mixed_site()
    } else {
        panic!("unexpected hygiene {hygiene}")
    };
    let mut output: Vec<_> = "pub fn value() -> u32 { 1 }"
        .parse::<TokenStream>()
        .expect("function tokens")
        .into_iter()
        .collect();
    for token in &mut output {
        match token {
            TokenTree::Ident(ident) => {
                if ident.to_string() == "value" {
                    ident.set_span(span);
                }
            }
            TokenTree::Group(_) => {}
            TokenTree::Literal(_) => {}
            TokenTree::Punct(_) => {}
        }
    }
    output.into_iter().collect()
}
