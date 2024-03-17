# Generate PDFs from given input

## Supported Inputs

 * CSV files,
 * Markdown (Commonmark with extensions, see [the markdown tex package](https://github.com/Witiko/markdown)),
 * Image files,
 * ConTeXt (MKIV) partial documents.


## Commandline Client

```
cargo run -- -t template.mkiv -i preface.md -i data.csv -o file.pdf
```
will render `preface.md` and `data.csv` with the given template `template.mkiv`.


## Web service

Is a a `axum`-based small web service that generates a PDF from the given inputs.

```
cargo run --bin web-service &
cat <<EOF | curl -XPOST --json @- http://localhost:8000/rendered_pdf
{
  "inputs": [
     {  "literal": "# title\n\nsome text",
        "type": "text/markdown"
     },
     {
        "s3_file": "a_s3_presigned_url",
        "type", "text/csv"
     }
  ],
  "output": {
    "url": "a_s3_presigned_url"
  }
}
<<EOF
```

## Storing/Reading files in S3 (compatible blob stores)

The library can use files stored in S3.
You'll have to pass a presigned URL (for reading or writing) though.


## How it works

It collects all input files in a temporary directory, creates a ConTeXt MKIV file that references the files and compiles the file with the `context` tool.
Afterwards it copies the resulting file to the output and cleans up.
