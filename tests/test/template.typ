#let tocm(size) = {
  return calc.round(size/1cm, digits: 2)
}

#let case(id, size: 10pt, outset: 0pt) = {
  locate(loc => {
    let l = loc.position()
    let bb = (l.page,(tocm(l.x),tocm(l.x+size)),(tocm(l.y),tocm(l.y+size)))
    write_json("/pos.json", id, bb)
    square(size: size, outset: outset)
  })
}