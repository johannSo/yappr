import Overlay from "./Overlay";

// The whole app is the overlay pill (spec 12). There is no other window,
// no other view, and no pipeline logic here -- see Overlay.tsx's module
// comment.
function App() {
  return <Overlay />;
}

export default App;
