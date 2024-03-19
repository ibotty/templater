# Templating

The templating is done using [minijinja](https://docs.rs/minijinja/latest/minijinja/).


## Special variables

The following special variables are set by default to be used in templates.

* `__assets_path` points to the assets that in the server are expected in `/etc/templater/assets`.
* `__templatet_path` points to the template files.  Note, that it is rarely neccessary to use it.  Jinja partials don't need to use this path..
