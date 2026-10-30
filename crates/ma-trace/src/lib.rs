//! Attribut `#[covers("ID", ...)]` : relie un test aux exigences qu'il couvre.
//!
//! L'attribut ne modifie pas l'élément annoté ; il est lu par `cargo xtask trace` pour construire la
//! matrice de traçabilité. Il refuse à la compilation une liste vide ou un identifiant mal formé.

use proc_macro::TokenStream;
use syn::punctuated::Punctuated;
use syn::{LitStr, Token, parse_macro_input};

/// Vrai si `id` respecte `^[A-Z][A-Z0-9]*(\.[A-Z0-9]+)*\.[0-9]{2}$`.
fn valid_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    let Some((last, head)) = parts.split_last() else {
        return false;
    };
    if head.is_empty() || last.len() != 2 || !last.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let first = head[0].as_bytes();
    if first.is_empty() || !first[0].is_ascii_uppercase() {
        return false;
    }
    head.iter().all(|p| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    })
}

/// Relie la fonction annotée aux identifiants d'exigences donnés.
#[proc_macro_attribute]
pub fn covers(attr: TokenStream, item: TokenStream) -> TokenStream {
    let ids = parse_macro_input!(attr with Punctuated::<LitStr, Token![,]>::parse_terminated);
    if ids.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[covers] attend au moins un identifiant d'exigence",
        )
        .to_compile_error()
        .into();
    }
    // Seules les fonctions libres sont tracées (les tests ne sont ni des méthodes ni d'autres éléments).
    let span = proc_macro2::TokenStream::from(item.clone());
    match syn::parse::<syn::ItemFn>(item.clone()) {
        Ok(f) if f.sig.receiver().is_none() => {}
        _ => {
            return syn::Error::new_spanned(span, "#[covers] ne s'applique qu'à une fonction (test), pas à une méthode ni à un autre élément")
                .to_compile_error()
                .into();
        }
    }
    for id in &ids {
        if !valid_id(&id.value()) {
            return syn::Error::new(
                id.span(),
                format!("identifiant d'exigence invalide : {:?}", id.value()),
            )
            .to_compile_error()
            .into();
        }
    }
    item
}
