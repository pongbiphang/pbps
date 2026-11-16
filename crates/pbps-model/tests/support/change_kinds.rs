//! The kinds of `Change`, read from change.rs so a list cannot fall behind the enum.

/// The variants of `Change` with each family variant (`Extension(ExtensionChange)`)
/// replaced by the kinds of the enum it holds (#2790). A variant counts as a
/// family when it holds exactly one unboxed path type that is an enum of the
/// same file.
pub fn change_kinds(file: &syn::File) -> Vec<String> {
    let enums: Vec<&syn::ItemEnum> = file
        .items
        .iter()
        .filter_map(|item| {
            if let syn::Item::Enum(e) = item {
                Some(e)
            } else {
                None
            }
        })
        .collect();
    let change = enums
        .iter()
        .find(|e| e.ident == "Change")
        .expect("`Change` is an enum in change.rs");
    let mut kinds = Vec::new();
    for variant in &change.variants {
        let held = if let syn::Fields::Unnamed(f) = &variant.fields
            && f.unnamed.len() == 1
            && let syn::Type::Path(p) = &f.unnamed[0].ty
        {
            p.path.get_ident()
        } else {
            None
        };
        let family = held.and_then(|name| enums.iter().find(|e| e.ident == *name));
        match family {
            Some(e) => kinds.extend(e.variants.iter().map(|v| v.ident.to_string())),
            None => kinds.push(variant.ident.to_string()),
        }
    }
    kinds
}
