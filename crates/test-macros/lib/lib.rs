//! `#[mk_test]`: marks a test that needs a real microVM.
//!
//! Expands to `#[tokio::test(flavor = "multi_thread")] #[ignore]` and calls
//! `test_utils::init_isolated_home()` first, mirroring microsandbox's
//! `#[msb_test]`. Run these with `just test-integration`.

use proc_macro::TokenStream;
use proc_macro2::{Delimiter, Group, TokenTree};
use quote::quote;

#[proc_macro_attribute]
pub fn mk_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return quote! { compile_error!("#[mk_test] takes no arguments"); }.into();
    }

    let mut tokens: Vec<TokenTree> = proc_macro2::TokenStream::from(item).into_iter().collect();
    let body = match tokens.pop() {
        Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => group,
        _ => return quote! { compile_error!("#[mk_test] must be applied to a function"); }.into(),
    };

    let inner = body.stream();
    let mut new_body = Group::new(
        Delimiter::Brace,
        quote! {
            ::test_utils::init_isolated_home();
            #inner
        },
    );
    new_body.set_span(body.span());
    tokens.push(TokenTree::Group(new_body));

    let function: proc_macro2::TokenStream = tokens.into_iter().collect();
    quote! {
        #[::tokio::test(flavor = "multi_thread")]
        #[ignore = "needs a microVM; run with `just test-integration`"]
        #function
    }
    .into()
}
