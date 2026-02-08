use crate::input::OutputFileSpec;
use crate::models::ModelEntry;

/// Build the system prompt including file output instructions.
pub fn build_system_prompt(model_entry: &ModelEntry, output_files: &[OutputFileSpec]) -> String {
    let mut system_prompt = model_entry.system_prompt.clone().unwrap_or_default();

    if !output_files.is_empty() {
        let file_list: Vec<&str> = output_files.iter().map(|f| f.filename.as_str()).collect();
        let file_instructions = format!(
            "IMPORTANT: You have access to a `save_file` tool for writing files. \
             The user has marked these files for output using !filename syntax: {}\n\
             \n\
             When you see !filename or @!filename in the user's prompt, that is just a marker. \
             You MUST use the save_file tool to write the actual content. \
             Use the tool with the actual filename (without the ! prefix).\n\
             \n\
             Example: If user says 'write hello to !test.txt', call save_file with path='test.txt' and content='hello'.\n\
             \n\
             Only write to files explicitly listed above - never write to any other files.",
            file_list.join(", ")
        );

        if system_prompt.is_empty() {
            system_prompt = file_instructions;
        } else {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&file_instructions);
        }
    }

    system_prompt
}
