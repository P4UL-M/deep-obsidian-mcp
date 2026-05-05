pub mod text;
pub mod vault;

pub use text::{
    count_terms, extract_block_sections, extract_heading_sections, extract_wiki_links,
    frontmatter_title, heading_title, normalize_heading_slug, note_title, tokenize, BlockSection,
    HeadingSection,
};
pub use vault::{
    chunk_lines, ensure_inside_vault, ensure_vault_path, list_children, list_folders,
    list_markdown_files, list_top_level_folders, read_text_file, slice_lines, write_binary_file,
    write_text_file, ChunkSection, ReadTextFileResult, VaultChildEntry, VaultEntryKind, VaultError,
    WriteBinaryFileResult, WriteTextFileResult, DEFAULT_IGNORED_DIRS,
};
