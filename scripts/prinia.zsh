# prinia shell integration. Type a request, press the key (default ctrl-g): the line becomes a
# command you can edit and run. Nothing runs until you press enter.
_prinia_widget() {
  [[ -z $BUFFER ]] && return
  local out
  zle -M "prinia: thinking..."
  if out=$(command prinia --cmd -- "$BUFFER" 2>/dev/null) && [[ -n $out ]]; then
    BUFFER=$out
    CURSOR=${#BUFFER}
    zle -M ""
  else
    zle -M "prinia: no answer (is your model running?)"
  fi
}
zle -N _prinia_widget
# Keys: PRINIA_WIDGET_KEY, a space-separated list (default ^g), bound in emacs mode and both vi modes.
for _prinia_map in emacs viins vicmd; do
  for _prinia_key in ${=PRINIA_WIDGET_KEY:-^g}; do
    bindkey -M $_prinia_map "$_prinia_key" _prinia_widget 2>/dev/null
  done
done
unset _prinia_map _prinia_key
